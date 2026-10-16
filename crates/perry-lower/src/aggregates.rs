//! Arrays and records: literals, element and field writes.
use super::*;
use crate::coerce::merge_absence;

impl<'s> FunctionBuilder<'s> {
    /// `array[index] = value`; computed keys on records are rejected (subset rule R1).
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
            && record.deref(ctx).is_tuple()
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
            if ty
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|int| int.width() == 64)
            {
                let bigint = BigIntType::get(ctx).into();
                return Ok(self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, bigint)));
            }
            return Ok(value);
        }
        ensure!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
        if let Some((k_ty, v_ty, tuple_rec)) =
            crate::collections::dict_entry_types(ctx, array.get_type(ctx))
        {
            let key = self.expression_with(ctx, index, Some(k_ty))?;
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            return self.kv_set(ctx, array, key, val, k_ty, v_ty, tuple_rec);
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
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        if let Some((k_ty, v_ty, tuple_rec)) =
            crate::collections::dict_entry_types(ctx, record.get_type(ctx))
        {
            let key = self.string(ctx, field);
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            return self.kv_set(ctx, record, key, val, k_ty, v_ty, tuple_rec);
        }
        ensure!(
            kind(ctx, record)? == Kind::Record,
            "property writes are only supported on records so far"
        );
        let field_type = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?
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
        if field_type
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|int| int.width() == 64)
        {
            let bigint = BigIntType::get(ctx).into();
            return Ok(self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, value, bigint)));
        }
        Ok(value)
    }

    /// `{ a: …, b: … }`: evaluate fields in source order, allocate in record order.
    pub(super) fn record_literal(
        &mut self,
        ctx: &mut Context,
        names: &[&str],
        args: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        // A record or variant where `T | undefined` is expected is a `T`.
        let expected = expected.map(|ty| optional_value(ctx, ty).unwrap_or(ty));
        if let Some(ty) = expected
            && ty.deref(ctx).is::<perry_mir::VariantType>()
        {
            return self.variant_literal(ctx, ty, names, args);
        }
        if let Some(ty) = expected
            && let Some((k_ty, v_ty, tuple_rec)) = crate::collections::dict_entry_types(ctx, ty)
        {
            return self.dict_literal(ctx, names, args, k_ty, v_ty, tuple_rec);
        }
        if let Some(record) =
            expected.and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
        {
            let missing: Vec<_> = record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .iter()
                .filter(|(name, ty)| {
                    !names.contains(&name.as_str())
                        && !absence(ctx, *ty).is_some_and(|states| states.admits_undefined())
                })
                .map(|(name, _)| name.to_string())
                .collect();
            if !missing.is_empty() {
                return Err(PerryDiagnostic::error(
                    DiagnosticCode::MissingRequiredField,
                    format!("missing required record fields: {}", missing.join(", ")),
                )
                .into());
            }
        }
        let expected = expected
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
            .filter(|record| {
                record.deref(ctx).fields().is_ok_and(|fields| {
                    names
                        .iter()
                        .all(|name| fields.iter().any(|(field, _)| field.as_str() == *name))
                        && fields.iter().all(|(name, ty)| {
                            names.contains(&name.as_str())
                                || absence(ctx, *ty).is_some_and(|states| states.admits_undefined())
                        })
                })
            });
        let mut values = Vec::with_capacity(args.len());
        for (&name, arg) in names.iter().zip(args) {
            let field_type = match expected {
                Some(record) => record.deref(ctx).field(name).map(|(_, ty)| ty),
                _ => None,
            };
            let value = self.expression_with(ctx, arg, field_type)?;
            let value = match field_type {
                Some(field_type) => self.fit(ctx, value, field_type)?,
                None => value,
            };
            values.push((name, value));
        }
        // With a matching expected type the fields were fitted to it; that includes recursive
        // records, which no literal's own structure can name. Its fields the literal omits
        // are `undefined`, by record position.
        let (record, missing) = match expected {
            Some(record) => {
                let missing: Vec<(usize, TypeHandle)> = record
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .iter()
                    .enumerate()
                    .filter(|(_, (name, _))| !names.contains(&name.as_str()))
                    .map(|(index, (_, ty))| (index, *ty))
                    .collect();
                (record, missing)
            }
            None => {
                let fields = values
                    .iter()
                    .map(|&(name, value)| (name.to_string(), value.get_type(ctx)))
                    .collect();
                (self.shapes.mir_record(ctx, fields)?, Vec::new())
            }
        };
        let mut positioned = Vec::with_capacity(values.len() + missing.len());
        for (index, ty) in missing {
            positioned.push((
                index,
                self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty)),
            ));
        }
        for (name, value) in values {
            let (index, _) = record
                .deref(ctx)
                .field(name)
                .with_context(|| format!("record has no field {name}"))?;
            positioned.push((index, value));
        }
        positioned.sort_by_key(|(index, _)| *index);
        let operands = positioned.into_iter().map(|(_, value)| value).collect();
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, operands)))
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
        self.record_literal(ctx, names, args, expected)
    }

    /// `[…]`, with the element type from `expected` or from the items.
    pub(super) fn array_literal(
        &mut self,
        ctx: &mut Context,
        items: &'s [Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let element = match expected.map(|ty| {
            TypedHandle::<ArrayType>::from_handle(optional_value(ctx, ty).unwrap_or(ty), ctx)
        }) {
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
                let inner = optional_value(ctx, first).unwrap_or(first);
                let mut states = absence(ctx, first);
                for value in rest {
                    let ty = value.get_type(ctx);
                    ensure!(
                        optional_value(ctx, ty).unwrap_or(ty) == inner,
                        "array literals must have one element type"
                    );
                    states = merge_absence(states, absence(ctx, ty));
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

    /// `local.push(value)` on an array local.
    pub(super) fn array_push(
        &mut self,
        ctx: &mut Context,
        array_id: LocalId,
        value: &'s Expr,
    ) -> Result<Value> {
        let slot = self.local(array_id)?;
        let array = self.load(ctx, slot)?;
        self.push_values(ctx, array, std::slice::from_ref(value))
    }

    /// `array.push(…values)`: every value is evaluated and fitted to the element type before
    /// the first push; the result is the new length.
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
