//! Arrays and records: literals, element and field writes.
mod records;

use super::*;
use crate::coerce::merge_absence;

impl<'s> FunctionBuilder<'s> {
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
        if let Ok(record) = TypedHandle::<RecordType>::from_handle(array.get_type(ctx), ctx)
            && record.deref(ctx).kind() == RecordKind::Tuple
        {
            let field = match index {
                Expr::Number(index) if *index >= 0.0 && index.fract() == 0.0 => {
                    format!("_{}", *index as u64)
                }
                Expr::Integer(index) if *index >= 0 => format!("_{index}"),
                _ => bail!("tuples require literal positions"),
            };
            let ty = record
                .deref(ctx)
                .field(&field)
                .map(|(_, ty)| ty)
                .context("tuple index is outside its declared shape")?;
            let value = self.expression_with(ctx, value, Some(ty))?;
            let value = self.fit(ctx, value, ty)?;
            self.emit(ctx, |ctx| FieldSetOp::new(ctx, array, field, value));
            return Ok(self.bigint_view(ctx, value));
        }
        ensure!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
        if let Some((k_ty, v_ty, _)) =
            crate::collections::dict_entry_types(ctx, array.get_type(ctx))
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

    pub(super) fn set_field(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        field: &str,
        value: &'s Expr,
    ) -> Result<Value> {
        if let Expr::ClassRef(class_name) = object {
            return self.class_static_field_set(ctx, class_name, field, value);
        }
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        let record = if crate::value_unions::is_value_union(ctx, record.get_type(ctx)) {
            let payload = perry_mir::variant_cases(ctx, record.get_type(ctx))?
                .into_iter()
                .find_map(|(tag, payload)| (tag == "object").then_some(payload).flatten())
                .context("property write requires a record alternative")?;
            self.emit_value(ctx, |ctx| {
                perry_mir::VariantPayloadOp::new(ctx, record, payload)
            })
        } else {
            record
        };

        if let Some((_, v_ty, _)) = crate::collections::dict_entry_types(ctx, record.get_type(ctx))
        {
            let key = self.string(ctx, field);
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            let val = self.fit(ctx, val, v_ty)?;
            self.kv_set(ctx, record, key, val)?;
            return Ok(val);
        }
        if let Some((class_name, type_args)) = self.shapes.class_of_type(ctx, record.get_type(ctx))
            && let Some((owner_class, setter)) = self.shapes.class_setter(class_name, field)
        {
            return self.class_setter_call(ctx, record, owner_class, &type_args, setter, value);
        }
        let rec_ty = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .ok()
            .filter(|rec| rec.deref(ctx).kind() == RecordKind::Object)
            .ok_or_else(|| anyhow!("property writes are only supported on records so far"))?;
        let field_type = rec_ty
            .deref(ctx)
            .field(field)
            .map(|(_, ty)| ty)
            .ok_or_else(|| {
                anyhow!("property {field} does not exist on this shape (subset rule R1)")
            })?;
        let value = self.expression_with(ctx, value, Some(field_type))?;
        let value = self.fit(ctx, value, field_type)?;
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, record, field.to_string(), value)
        });
        Ok(self.bigint_view(ctx, value))
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
            .with_context(|| format!("class {class_name} is not supported yet"))?;
        ensure!(
            names.len() == args.len(),
            "object literal arity differs from its shape"
        );
        self.record_literal(ctx, names, args, expected, RecordKind::Object)
    }

    /// `[…]`, with the element type from `expected` or from the items.
    pub(super) fn array_literal(
        &mut self,
        ctx: &mut Context,
        items: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let element = match expected
            .map(|ty| TypedHandle::<ArrayType>::from_handle(present_type(ctx, ty), ctx))
        {
            Some(Ok(array)) => Some(array.deref(ctx).element()),
            _ => None,
        };
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
                    states = merge_absence(states, absence(ctx, ty));
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
            (None, None) => bail!("an empty array literal needs a declared element type"),
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

    pub(super) fn peek_record_type(
        &self,
        ctx: &Context,
        expr: &Expr,
    ) -> Option<TypedHandle<RecordType>> {
        let record = match expr {
            Expr::LocalGet(id) => {
                let local = self.locals.get(id)?;
                let ty = self.slot_type(ctx, local.slot).ok()?;
                let inner = present_type(ctx, ty);
                TypedHandle::<RecordType>::from_handle(inner, ctx).ok()?
            }
            Expr::PropertyGet {
                object, property, ..
            } => {
                let rec = self.peek_record_type(ctx, object)?;
                let (_, field_ty) = rec.deref(ctx).field(property)?;
                let inner = present_type(ctx, field_ty);
                TypedHandle::<RecordType>::from_handle(inner, ctx).ok()?
            }
            Expr::Call { callee, .. } => {
                if let Expr::FuncRef(id) = &**callee
                    && let Some(sig) = self.signatures.get(id)
                    && let Some(ret) = sig.result
                {
                    let inner = present_type(ctx, ret.carrier);
                    TypedHandle::<RecordType>::from_handle(inner, ctx).ok()?
                } else {
                    return None;
                }
            }
            _ => return None,
        };
        (record.deref(ctx).kind() == RecordKind::Object).then_some(record)
    }
}
