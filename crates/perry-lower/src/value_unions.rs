use super::*;
use perry_mir::{VariantPayloadOp, VariantTagOp};

pub(super) fn is_value_union(ctx: &Context, ty: TypeHandle) -> bool {
    let ty = present_type(ctx, ty).deref(ctx);
    ty.is::<perry_mir::ValueUnionType>() || ty.is::<perry_mir::NumberStringType>()
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
        let cases = perry_mir::variant_cases(ctx, value.get_type(ctx))?;
        let tag = self.emit_value(ctx, |ctx| VariantTagOp::new(ctx, value));
        let output = self.entry_slot(ctx, result);
        let done = self.new_block(ctx, "union_join");
        for (index, (name, payload)) in cases.iter().enumerate() {
            let payload = payload.context("value union without a payload")?;
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
        let record = TypedHandle::<RecordType>::from_handle(value.get_type(ctx), ctx)?;
        ensure!(
            self.shapes
                .class_of_type(ctx, value.get_type(ctx))
                .is_none()
                && record.deref(ctx).field("toString").is_none()
                && record.deref(ctx).field("valueOf").is_none(),
            "custom primitive conversion is unsupported in value unions"
        );
        Ok(self.string(ctx, "[object Object]"))
    }

    pub(super) fn value_union_member(
        &mut self,
        ctx: &mut Context,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        let cases = perry_mir::variant_cases(ctx, value.get_type(ctx))?;
        let mut result = None;
        for (_, payload) in &cases {
            let field_type = payload.and_then(|ty| {
                let ty = ty.deref(ctx);
                if let Some(record) = ty.downcast_ref::<RecordType>() {
                    record.field(field).map(|(_, ty)| ty)
                } else if ty.is::<StringType>() && field == "length" {
                    Some(FP64Type::get(ctx).into())
                } else {
                    None
                }
            });
            if let Some(ty) = field_type {
                ensure!(
                    result.is_none_or(|other| other == ty),
                    "union property needs one value type"
                );
                result = Some(ty);
            }
        }
        let result = result.context("property does not exist on any union member")?;
        let result = crate::coerce::admitting(ctx, result, perry_mir::Absence::Undefined);
        self.dispatch_value(ctx, value, result, &|b, ctx, payload| {
            let has_field = payload
                .get_type(ctx)
                .deref(ctx)
                .downcast_ref::<RecordType>()
                .is_some_and(|record| record.field(field).is_some());
            if has_field {
                let op = FieldGetOp::new(ctx, payload, field.to_owned())?;
                b.field_read(ctx, &op)
            } else if payload.get_type(ctx).deref(ctx).is::<StringType>() && field == "length" {
                let len = b.emit_value(ctx, |ctx| StringLenOp::new(ctx, payload));
                Ok(b.emit_value(ctx, |ctx| {
                    NumericCastOp::new(ctx, len, FP64Type::get(ctx).into())
                }))
            } else {
                b.absent_literal(ctx, false, Some(result))
            }
        })
    }
}
