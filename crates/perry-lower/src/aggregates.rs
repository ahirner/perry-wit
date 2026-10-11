//! Arrays and records: literals, element and field writes.
use super::*;

impl FunctionBuilder<'_> {
    /// `array[index] = value`; computed keys on records are rejected (subset rule R1).
    pub(super) fn set_index(
        &mut self,
        ctx: &mut Context,
        object: &Expr,
        index: &Expr,
        value: &Expr,
    ) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        ensure!(
            kind(ctx, array)? == Kind::Array,
            "computed property keys are only supported on arrays (subset rule R1)"
        );
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
        object: &Expr,
        field: &str,
        value: &Expr,
    ) -> Result<Value> {
        let record = self.expression(ctx, object)?;
        let record = self.defined(ctx, record)?;
        ensure!(
            kind(ctx, record)? == Kind::Record,
            "property writes are only supported on records so far"
        );
        let field = field.to_string();
        let field_type = TypedHandle::<RecordType>::from_handle(record.get_type(ctx), ctx)
            .map_err(|_| anyhow!("expected a record"))?
            .deref(ctx)
            .field(&field)
            .map(|(_, ty)| ty)
            .ok_or_else(|| {
                anyhow!("property {field} does not exist on this shape (subset rule R1)")
            })?;
        let value = self.expression_with(ctx, value, Some(field_type))?;
        let value = self.fit(ctx, value, field_type)?;
        self.emit(ctx, |ctx| FieldSetOp::new(ctx, record, field, value));
        Ok(value)
    }

    /// `{ a: …, b: … }`: evaluate fields in source order, allocate in record order.
    pub(super) fn record_literal(
        &mut self,
        ctx: &mut Context,
        names: &[&str],
        args: &[Expr],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        if let Some(ty) = expected.map(|ty| optional_value(ctx, ty).unwrap_or(ty))
            && ty.deref(ctx).is::<perry_mir::types::VariantType>()
        {
            return self.variant_literal(ctx, ty, names, args);
        }
        let mut wanted = names.to_vec();
        wanted.sort_unstable();
        // A record where `T | undefined` is expected is a `T`.
        let expected = expected
            .map(|ty| optional_value(ctx, ty).unwrap_or(ty))
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok())
            .filter(|record| {
                record.deref(ctx).fields().is_ok_and(|fields| {
                    wanted
                        .iter()
                        .all(|name| fields.iter().any(|(field, _)| field.as_str() == *name))
                        && fields.iter().all(|(name, ty)| {
                            wanted.contains(&name.as_str())
                                || absence(ctx, *ty).is_some_and(|states| states.admits_undefined())
                        })
                })
            });
        let mut values = Vec::with_capacity(args.len());
        for (name, arg) in names.iter().zip(args) {
            let field_type = match expected {
                Some(record) => record.deref(ctx).field(name).map(|(_, ty)| ty),
                _ => None,
            };
            let value = self.expression_with(ctx, arg, field_type)?;
            let value = match field_type {
                Some(field_type) => self.fit(ctx, value, field_type)?,
                None => value,
            };
            values.push((name.to_string(), value));
        }
        // With a matching expected type the fields were fitted to it; that includes recursive
        // records, which no literal's own structure can name.
        let record = match expected {
            Some(record) => {
                let fields = record
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .to_vec();
                for (name, ty) in fields {
                    if !names.contains(&name.as_str()) {
                        let value = self.emit_value(ctx, |ctx| UndefinedOp::with_type(ctx, ty));
                        values.push((name, value));
                    }
                }
                record
            }
            None => {
                let fields = values
                    .iter()
                    .map(|(n, v)| (n.clone(), v.get_type(ctx)))
                    .collect();
                self.shapes.mir_record(ctx, fields)?
            }
        };
        let mut positioned = values
            .into_iter()
            .map(|(name, value)| {
                let (index, _) = record
                    .deref(ctx)
                    .field(&name)
                    .with_context(|| format!("record has no field {name}"))?;
                Ok((index, value))
            })
            .collect::<Result<Vec<_>>>()?;
        positioned.sort_by_key(|(index, _)| *index);
        let operands = positioned.into_iter().map(|(_, value)| value).collect();
        Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, record, operands)))
    }
    pub(super) fn object_literal(
        &mut self,
        ctx: &mut Context,
        class_name: &str,
        args: &[Expr],
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
        self.record_literal(ctx, &names, args, expected)
    }
    pub(super) fn array_literal(
        &mut self,
        ctx: &mut Context,
        items: &[Expr],
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
        let element = match (element, values.first()) {
            (Some(element), _) => element,
            (None, Some(first)) => {
                let first = first.get_type(ctx);
                let inner = optional_value(ctx, first).unwrap_or(first);
                let mut states = absence(ctx, first);
                for value in &values[1..] {
                    let ty = value.get_type(ctx);
                    ensure!(
                        optional_value(ctx, ty).unwrap_or(ty) == inner,
                        "array literals must have one element type"
                    );
                    states = coerce::merge_absence(states, absence(ctx, ty));
                }
                states.map_or(inner, |states| {
                    perry_mir::types::OptionalType::get_with(ctx, inner, states).into()
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
    pub(super) fn array_push(
        &mut self,
        ctx: &mut Context,
        array_id: LocalId,
        value: &Expr,
    ) -> Result<Value> {
        let slot = self.local(array_id)?;
        let array = self.load(ctx, slot)?;
        let element = self.array_element(ctx, array)?;
        let value = self.expression_with(ctx, value, Some(element))?;
        let value = self.fit(ctx, value, element)?;
        self.emit(ctx, |ctx| ArrayPushOp::new(ctx, array, value));
        let length = self.emit_value(ctx, |ctx| ArrayLenOp::new(ctx, array));
        Ok(self.emit_value(ctx, |ctx| {
            ConvertOp::new(ctx, ConvertKind::I32ToF64, length)
        }))
    }
}
