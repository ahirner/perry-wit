use super::*;
use perry_mir::{VariantPayloadOp, VariantTagOp};

pub(super) fn is_value_union(ctx: &Context, ty: TypeHandle) -> bool {
    present_type(ctx, ty)
        .deref(ctx)
        .is::<perry_mir::ValueUnionType>()
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn dispatch_value(
        &mut self,
        ctx: &mut Context,
        value: Value,
        result: TypeHandle,
        action: &dyn Fn(&mut Self, &mut Context, Value) -> Result<Value>,
    ) -> Result<Value> {
        if self.is_optional(ctx, value) {
            return self.match_optional(
                ctx,
                value,
                result,
                |b, ctx| {
                    let absent = b.absent_literal(ctx, false, None)?;
                    action(b, ctx, absent)
                },
                |b, ctx| {
                    let absent = b.absent_literal(ctx, true, None)?;
                    action(b, ctx, absent)
                },
                |b, ctx, value| b.dispatch_value(ctx, value, result, action),
            );
        }
        if !is_value_union(ctx, value.get_type(ctx)) {
            return action(self, ctx, value);
        }
        let cases = perry_mir::variant_cases(ctx, value.get_type(ctx)).to_diag(ctx)?;
        let tag = self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value));
        let output = self.entry_slot(ctx, result);
        let done = self.new_block(ctx, "union_join");
        for (index, (name, payload)) in cases.iter().enumerate() {
            let payload =
                payload.ok_or_else(|| Diagnostic::internal("value union without a payload"))?;
            let next = if index + 1 < cases.len() {
                let expected = self.string(ctx, name);
                let matches = self.compare(ctx, CompareKind::Eq, tag, expected);
                let active = self.new_block(ctx, "union_case");
                let next = self.new_block(ctx, "union_next");
                self.branch(ctx, matches, active, next);
                self.switch_to(active);
                Some(next)
            } else {
                None
            };
            let payload = self.emit_value(ctx, |ctx| VariantPayloadOp::new(ctx, value, payload));
            let value = action(self, ctx, payload)?;
            let value = self.fit(ctx, value, result)?;
            self.store(ctx, output, value);
            self.jump(ctx, done);
            if let Some(next) = next {
                self.switch_to(next);
            }
        }
        self.switch_to(done);
        self.load(ctx, output)
    }

    pub(super) fn value_primitive(&mut self, ctx: &mut Context, value: Value) -> Result<Value> {
        if Kind::of(ctx, present_type(ctx, value.get_type(ctx))) != Some(Kind::Record) {
            return Ok(value);
        }
        let record =
            TypedHandle::<RecordType>::from_handle(value.get_type(ctx), ctx).to_diag(ctx)?;
        ensure_unsupported!(
            self.shapes
                .class_of_type(ctx, value.get_type(ctx))
                .is_none()
                && record.deref(ctx).field("toString").is_none()
                && record.deref(ctx).field("valueOf").is_none(),
            "custom primitive conversion is unsupported in value unions"
        );
        Ok(self.string(ctx, "[object Object]"))
    }

    pub(super) fn byte_input_method(
        &mut self,
        ctx: &mut Context,
        value: Value,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        use crate::methods::{Call, Receiver};
        let text = Call::parse(Receiver::Value(Kind::String), method, args)?;
        let bytes = Call::parse(Receiver::Value(Kind::Bytes), method, args)?;
        let (tag, ty, call) = match (text, bytes) {
            (Some(_), Some(_)) if method == "slice" => {
                return self
                    .dispatch_value(ctx, value, value.get_type(ctx), &|b, ctx, item| {
                        let call = Call::parse(Receiver::of(ctx, item)?, method, args)?
                            .ok_or_else(|| internal!("missing slice method"))?;
                        call.lower(b, ctx, item)?
                            .ok_or_else(|| internal!("slice has no result"))
                    })
                    .map(Some);
            }
            (Some(_), None) if matches!(method, "at" | "indexOf" | "lastIndexOf" | "includes") => {
                bail_unsupported!("narrow the string or Uint8Array before calling {method}")
            }
            (Some(call), None) => ("string", StringType::get(ctx).into(), call),
            (None, Some(call)) => ("object", BytesType::get(ctx).into(), call),
            _ => bail_unsupported!("method {method} is not supported on a string or Uint8Array"),
        };
        let actual = self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value));
        let tag = self.string(ctx, tag);
        let matches = self.compare(ctx, CompareKind::Eq, actual, tag);
        let invoke = self.new_block(ctx, "method");
        let missing = self.new_block(ctx, "missing_method");
        self.branch(ctx, matches, invoke, missing);
        self.switch_to(missing);
        for arg in args {
            self.effect(ctx, arg)?;
        }
        self.raise_error(
            ctx,
            ErrorKind::TypeError,
            &format!("{method} is not a function"),
        );
        self.jump(ctx, invoke);
        self.switch_to(invoke);
        let item = self.fit(ctx, value, ty)?;
        call.lower(self, ctx, item)
    }

    pub(super) fn byte_input_index(
        &mut self,
        ctx: &mut Context,
        value: Value,
        index: Value,
    ) -> Result<Value> {
        let result = self.shapes.value_type(
            ctx,
            &HirType::Union(vec![HirType::Number, HirType::String, HirType::Void]),
        )?;
        self.dispatch_value(ctx, value, result, &|b, ctx, item| {
            let get = if item.get_type(ctx).deref(ctx).is::<StringType>() {
                Intrinsic::StringIndex
            } else {
                Intrinsic::BytesGet
            };
            Ok(b.intrinsic(ctx, get, vec![item, index]))
        })
    }

    pub(super) fn value_union_member(
        &mut self,
        ctx: &mut Context,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        let cases = perry_mir::variant_cases(ctx, value.get_type(ctx)).to_diag(ctx)?;
        let mut result = None;
        let mut missing = false;
        for (_, payload) in &cases {
            let field_type = payload.and_then(|ty| {
                let ty = ty.deref(ctx);
                if let Some(record) = ty.downcast_ref::<RecordType>() {
                    record.field(field).map(|(_, ty)| ty)
                } else if ty.is::<BytesType>() && field == "buffer" {
                    Some(perry_mir::ByteBufferType::get(ctx).into())
                } else if (ty.is::<StringType>() && field == "length")
                    || (ty.is::<BytesType>()
                        && matches!(field, "length" | "byteLength" | "byteOffset"))
                {
                    Some(FP64Type::get(ctx).into())
                } else {
                    None
                }
            });
            if let Some(ty) = field_type {
                ensure_unsupported!(
                    result.is_none_or(|other| other == ty),
                    "union property needs one value type"
                );
                result = Some(ty);
            } else {
                missing = true;
            }
        }
        let result = result.ok_or_else(|| {
            Diagnostic::new(
                codes::TYPE_MISMATCH,
                "property does not exist on any union member",
            )
        })?;
        let result = if missing {
            crate::coerce::admitting(ctx, result, perry_mir::Absence::Undefined)
        } else {
            result
        };
        self.dispatch_value(ctx, value, result, &|b, ctx, payload| {
            let has_field = payload
                .get_type(ctx)
                .deref(ctx)
                .downcast_ref::<RecordType>()
                .is_some_and(|record| record.field(field).is_some());
            if has_field {
                let op = FieldGetOp::new(ctx, payload, field.to_owned()).to_diag(ctx)?;
                b.field_read(ctx, &op)
            } else if payload.get_type(ctx).deref(ctx).is::<BytesType>() {
                match field {
                    "length" | "byteLength" => {
                        Ok(b.intrinsic(ctx, Intrinsic::BytesLength, vec![payload]))
                    }
                    "byteOffset" => Ok(b.intrinsic(ctx, Intrinsic::BytesOffset, vec![payload])),
                    "buffer" => Ok(b.intrinsic(ctx, Intrinsic::BytesBuffer, vec![payload])),
                    _ => b.absent_literal(ctx, false, Some(result)),
                }
            } else if payload.get_type(ctx).deref(ctx).is::<StringType>() && field == "length" {
                Ok(b.string_length(ctx, payload))
            } else {
                b.absent_literal(ctx, false, Some(result))
            }
        })
    }
}
