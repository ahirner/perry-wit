//! Arrays and records: literals, element and field writes.
mod records;

use super::*;
use expressions::{Assigned, Member};

impl<'s> FunctionBuilder<'s, '_> {
    pub(super) fn set_byte_index(
        &mut self,
        ctx: &mut Context,
        bytes: Value,
        index: &'s Expr,
        value: &'s Expr,
    ) -> Result<Value> {
        let index = self.number_operand(ctx, index)?;
        let value = self.number_operand(ctx, value)?;
        self.intrinsic_void(ctx, Intrinsic::BytesSet, vec![bytes, index, value]);
        Ok(value)
    }

    /// Lower `array[index] = value` for arrays and tuples.
    pub(super) fn set_index(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        index: &'s Expr,
        value: &'s Expr,
    ) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        if array.get_type(ctx).deref(ctx).is::<BytesType>() {
            return self.set_byte_index(ctx, array, index, value);
        }
        if let Some(record) = perry_mir::typed::<RecordType>(ctx, array.get_type(ctx))
            && record.deref(ctx).kind() == RecordKind::Tuple
        {
            let position = hir_compat::tuple_position(index)
                .ok_or_else(|| unsupported!("tuples require literal positions"))?;
            return Member::field(ctx, array, &format!("_{position}"), None)?
                .write(self, ctx, Assigned::Expression(value))?
                .ok_or_else(|| internal!("missing tuple field value"));
        }
        ensure_unsupported!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
        if let Some((k_ty, v_ty, _)) =
            crate::lower::collections::dict_entry_types(ctx, array.get_type(ctx))
        {
            let key = self.expression_with(ctx, index, Some(k_ty))?;
            let key = self.fit(ctx, key, k_ty)?;
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            let val = self.fit(ctx, val, v_ty)?;
            self.kv_set(ctx, array, key, val)?;
            return Ok(val);
        }
        let element = self.array_element(ctx, array)?;
        let index = self.index(ctx, index)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        let value = self.fit(ctx, value, element)?;
        self.emit(ctx, |ctx| ArraySetOp::new(ctx, array, index, value));
        Ok(value)
    }

    pub(super) fn capture_field(
        &mut self,
        ctx: &mut Context,
        value: Value,
        slot: usize,
    ) -> Result<Value> {
        let record = perry_mir::typed::<RecordType>(ctx, value.get_type(ctx))
            .ok_or_else(|| internal!("expected a capture record"))?;
        let name = record
            .deref(ctx)
            .fields()
            .to_diag(ctx)?
            .get(slot)
            .ok_or_else(|| internal!("missing capture slot {slot}"))?
            .0
            .clone();
        let op = FieldGetOp::new(ctx, value, name).to_diag(ctx)?;
        self.field_read(ctx, &op)
    }

    pub(super) fn record_field(
        &mut self,
        ctx: &mut Context,
        value: Value,
        name: &str,
    ) -> Result<Value> {
        let op = FieldGetOp::new(ctx, value, name.to_owned()).to_diag(ctx)?;
        self.field_read(ctx, &op)
    }

    pub(super) fn set_field(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        field: &str,
        value: &'s Expr,
        setter: Option<FuncId>,
    ) -> Result<Option<Value>> {
        if let Expr::ClassRef(class_name) = object {
            return self
                .local_set(ctx, self.shapes.static_field(class_name, field)?, value)
                .map(Some);
        }
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        if let Some(id) = setter {
            return Member::Accessor(record, id).write(self, ctx, Assigned::Expression(value));
        }
        let record = if crate::lower::value_unions::is_value_union(ctx, record.get_type(ctx)) {
            let payload = perry_mir::variant_cases(ctx, record.get_type(ctx))
                .to_diag(ctx)?
                .into_iter()
                .find_map(|(tag, payload)| (tag == "object").then_some(payload).flatten())
                .ok_or_else(|| {
                    diag!(
                        codes::TYPE_MISMATCH,
                        "property write requires a record alternative"
                    )
                })?;
            self.emit_value(ctx, |ctx| {
                perry_mir::VariantPayloadOp::new(ctx, record, payload)
            })
        } else {
            record
        };

        if let Some((_, v_ty, _)) =
            crate::lower::collections::dict_entry_types(ctx, record.get_type(ctx))
        {
            let key = self.string(ctx, field);
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            let val = self.fit(ctx, val, v_ty)?;
            self.kv_set(ctx, record, key, val)?;
            return Ok(Some(val));
        }
        Member::field(ctx, record, field, Some(RecordKind::Object))?.write(
            self,
            ctx,
            Assigned::Expression(value),
        )
    }

    /// `new C(…)` of a class that Perry HIR made from an object literal.
    pub(super) fn object_literal(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        args: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let names = self
            .shapes
            .literal_fields(class_name)
            .ok_or_else(|| unsupported!("class {class_name} is not supported yet"))?;
        ensure_internal!(
            names.len() == args.len(),
            "object literal arity differs from its shape"
        );
        let fields: Vec<(&str, &'s Expr)> = names
            .iter()
            .map(|field| field.name.as_str())
            .zip(args)
            .collect();
        self.record_literal(ctx, &fields, expected, RecordKind::Object)
    }

    /// `[…]`, with the element type from `expected` or from the items.
    pub(super) fn array_literal(
        &mut self,
        ctx: &mut Context,
        items: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let element = expected
            .and_then(|ty| perry_mir::typed::<ArrayType>(ctx, present_type(ctx, ty)))
            .map(|array| array.deref(ctx).element());
        let mut values = Vec::with_capacity(items.len());
        for item in items {
            let value = self.expression_with(ctx, item, element)?;
            let value = match element {
                Some(element) => self.fit(ctx, value, element)?,
                None => value,
            };
            values.push(value);
        }
        let element = match (element, values.split_first()) {
            (Some(element), _) => element,
            (None, Some((first, rest))) => {
                let first = first.get_type(ctx);
                let inner = present_type(ctx, first);
                let mut states = absence(ctx, first);
                let mut uniform = true;
                for value in rest {
                    let ty = value.get_type(ctx);
                    if present_type(ctx, ty) != inner {
                        uniform = false;
                        break;
                    }
                    states = perry_mir::Absence::join_optional(states, absence(ctx, ty));
                }
                if !uniform {
                    let fields: Vec<(String, TypeHandle)> = values
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (format!("_{i}"), v.get_type(ctx)))
                        .collect();
                    let record = self.shapes.mir_tuple(ctx, fields)?;
                    return Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, values)));
                }
                states.map_or(inner, |states| {
                    perry_mir::OptionalType::get_with(ctx, inner, states).into()
                })
            }
            (None, None) => bail_diag!(
                codes::MISSING_TYPE_ANNOTATION,
                "an empty array literal needs a declared element type"
            ),
        };
        let values = values
            .into_iter()
            .map(|v| self.fit(ctx, v, element))
            .collect::<Result<_>>()?;
        Ok(self.emit_value(ctx, |ctx| ArrayNewOp::new(ctx, element, values)))
    }

    /// Lower `array.push(…values)`, evaluating and coercing every argument to the element type
    /// before the first push and returning the new length.
    pub(super) fn push_values(
        &mut self,
        ctx: &mut Context,
        array: Value,
        values: &'s [Expr],
    ) -> Result<Value> {
        let element = self.array_element(ctx, array)?;
        let values = values
            .iter()
            .map(|value| {
                let value = self.expression_with(ctx, value, Some(element))?;
                self.fit(ctx, value, element)
            })
            .collect::<Result<Vec<_>>>()?;
        for value in values {
            self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, value));
        }
        Ok(self.array_length(ctx, array))
    }

    /// `array.length` as a number.
    pub(super) fn array_length(&mut self, ctx: &mut Context, array: Value) -> Value {
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
        self.emit_value(ctx, |ctx| {
            NumericCastOp::new(ctx, length, FP64Type::get(ctx).into())
        })
    }
}
