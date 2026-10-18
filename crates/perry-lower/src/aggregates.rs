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
        if let Some((k_ty, v_ty, _)) =
            crate::collections::dict_entry_types(ctx, array.get_type(ctx))
        {
            let key = self.expression_with(ctx, index, Some(k_ty))?;
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            return self.kv_set(ctx, array, key, val);
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
        if let Some((_, v_ty, _)) = crate::collections::dict_entry_types(ctx, record.get_type(ctx))
        {
            let key = self.string(ctx, field);
            let val = self.expression_with(ctx, value, Some(v_ty))?;
            return self.kv_set(ctx, record, key, val);
        }
        if let Some((class_name, type_args)) = self.shapes.class_of_type(ctx, record.get_type(ctx))
            && let Some((owner_class, setter)) = self.shapes.class_setter(class_name, field)
        {
            return self.class_setter_call(ctx, record, owner_class, &type_args, setter, value);
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
            && let Some((_, v_ty, tuple_rec)) = crate::collections::dict_entry_types(ctx, ty)
        {
            return self.dict_literal(ctx, names, args, v_ty, tuple_rec);
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
        let mut order = Vec::with_capacity(values.len());
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
            order.push(index as u32);
            positioned.push((index, value));
        }
        positioned.sort_by_key(|(index, _)| *index);
        let operands = positioned.into_iter().map(|(_, value)| value).collect();
        Ok(self.emit_value(ctx, |ctx| {
            let allocation = AllocOp::new(ctx, record, operands);
            allocation.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(order));
            allocation
        }))
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
                let mut uniform = true;
                for value in rest {
                    let ty = value.get_type(ctx);
                    if optional_value(ctx, ty).unwrap_or(ty) != inner {
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
                    let record = self.shapes.mir_record_kind(ctx, fields, true)?;
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

    pub(super) fn peek_record_type(
        &self,
        ctx: &Context,
        expr: &Expr,
    ) -> Option<TypedHandle<RecordType>> {
        match expr {
            Expr::LocalGet(id) => {
                let local = self.locals.get(id)?;
                let ty = self.slot_type(ctx, local.slot).ok()?;
                let inner = optional_value(ctx, ty).unwrap_or(ty);
                TypedHandle::<RecordType>::from_handle(inner, ctx).ok()
            }
            Expr::PropertyGet {
                object, property, ..
            } => {
                let rec = self.peek_record_type(ctx, object)?;
                let (_, field_ty) = rec.deref(ctx).field(property)?;
                let inner = optional_value(ctx, field_ty).unwrap_or(field_ty);
                TypedHandle::<RecordType>::from_handle(inner, ctx).ok()
            }
            Expr::Call { callee, .. } => {
                if let Expr::FuncRef(id) = &**callee
                    && let Some(sig) = self.signatures.get(id)
                    && let Some(ret) = sig.result
                {
                    let inner = optional_value(ctx, ret.carrier).unwrap_or(ret.carrier);
                    TypedHandle::<RecordType>::from_handle(inner, ctx).ok()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// `{ ...record, key: value }` over closed typed records.
    pub(super) fn record_object_spread(
        &mut self,
        ctx: &mut Context,
        parts: &'s [(Option<String>, Expr)],
        first_evaluated: Option<Value>,
        expected_record: Option<TypedHandle<RecordType>>,
    ) -> Result<Value> {
        let fallback_record = expected_record.or_else(|| {
            first_evaluated
                .and_then(|v| {
                    let ty = v.get_type(ctx);
                    let inner = optional_value(ctx, ty).unwrap_or(ty);
                    TypedHandle::<RecordType>::from_handle(inner, ctx).ok()
                })
                .or_else(|| {
                    parts.iter().find_map(|(k, v)| {
                        k.is_none().then(|| self.peek_record_type(ctx, v)).flatten()
                    })
                })
        });
        enum ResidueStep {
            Spread(Value),
            Insert(String),
        }
        let mut residue_steps: Vec<ResidueStep> = Vec::new();
        let mut field_values: Vec<(String, Value)> = Vec::new();
        for (idx, (key, val_expr)) in parts.iter().enumerate() {
            if let Some(key) = key {
                if expected_record.is_some_and(|rec| rec.deref(ctx).field(key).is_none())
                    && matches!(val_expr, Expr::Undefined)
                {
                    field_values.retain(|(k, _)| k != key);
                    residue_steps.push(ResidueStep::Insert(key.clone()));
                    continue;
                }
                let expected_field_ty = expected_record
                    .and_then(|rec| rec.deref(ctx).field(key).map(|(_, ty)| ty))
                    .or_else(|| {
                        if matches!(val_expr, Expr::Null | Expr::Undefined) {
                            field_values
                                .iter()
                                .find(|(k, _)| k == key)
                                .map(|(_, v)| v.get_type(ctx))
                                .or_else(|| {
                                    fallback_record
                                        .and_then(|rec| rec.deref(ctx).field(key).map(|(_, ty)| ty))
                                })
                                // Without a known field type the literal takes its own type.
                                .map(|base_ty| {
                                    let abs = if matches!(val_expr, Expr::Null) {
                                        perry_mir::Absence::Null
                                    } else {
                                        perry_mir::Absence::Undefined
                                    };
                                    crate::coerce::admitting(ctx, base_ty, abs)
                                })
                        } else {
                            None
                        }
                    });
                let val = self.expression_with(ctx, val_expr, expected_field_ty)?;
                let val = match expected_record
                    .and_then(|rec| rec.deref(ctx).field(key).map(|(_, ty)| ty))
                {
                    Some(target_ty) => self.fit(ctx, val, target_ty)?,
                    None => val,
                };
                residue_steps.push(ResidueStep::Insert(key.clone()));
                if let Some((_, existing)) = field_values.iter_mut().find(|(k, _)| k == key) {
                    let existing_ty = existing.get_type(ctx);
                    let val_ty = val.get_type(ctx);
                    if existing_ty == val_ty {
                        *existing = val;
                    } else if let Some(states) =
                        merge_absence(absence(ctx, existing_ty), absence(ctx, val_ty))
                    {
                        let inner = optional_value(ctx, val_ty)
                            .or_else(|| optional_value(ctx, existing_ty))
                            .unwrap_or(val_ty);
                        let merged_ty =
                            perry_mir::OptionalType::get_with(ctx, inner, states).into();
                        if expected_record.is_none() {
                            *existing = self.fit(ctx, val, merged_ty)?;
                        } else {
                            *existing = val;
                        }
                    } else {
                        *existing = val;
                    }
                } else {
                    field_values.push((key.clone(), val));
                }
            } else {
                let raw_src = if idx == 0
                    && let Some(first) = first_evaluated
                {
                    first
                } else {
                    self.expression(ctx, val_expr)?
                };
                if matches!(kind(ctx, raw_src), Ok(Kind::Exception)) {
                    continue;
                }
                let src = self.defined(ctx, raw_src)?;
                let src_rec = TypedHandle::<RecordType>::from_handle(src.get_type(ctx), ctx)
                    .context(
                        "object spread is supported over typed records or JSON values with static keys",
                    )?;
                residue_steps.push(ResidueStep::Spread(src));
                let src_fields: Vec<(String, TypeHandle)> = src_rec
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .to_vec();
                for (field_name, src_field_ty) in src_fields {
                    let can_omit = absence(ctx, src_field_ty).is_some_and(|a| a.admits_undefined());
                    let op = FieldGetOp::new(ctx, src, field_name.clone())?;
                    let loaded = self.field_read(ctx, &op)?;
                    let expected_field_ty = expected_record
                        .and_then(|rec| rec.deref(ctx).field(&field_name).map(|(_, ty)| ty));
                    if let Some((_, existing)) =
                        field_values.iter_mut().find(|(k, _)| k == &field_name)
                    {
                        if can_omit {
                            let prev = *existing;
                            let prev_ty = prev.get_type(ctx);
                            let loaded_ty = loaded.get_type(ctx);
                            let target_ty = expected_field_ty.unwrap_or_else(|| {
                                if let Some(states) =
                                    merge_absence(absence(ctx, prev_ty), absence(ctx, loaded_ty))
                                {
                                    let inner = optional_value(ctx, loaded_ty)
                                        .or_else(|| optional_value(ctx, prev_ty))
                                        .unwrap_or(loaded_ty);
                                    perry_mir::OptionalType::get_with(ctx, inner, states).into()
                                } else {
                                    loaded_ty
                                }
                            });
                            let op = FieldPresentOp::new(ctx, src, field_name.clone())?;
                            let is_present = self.emit_value(ctx, |_| op);
                            *existing = self.choose(
                                ctx,
                                target_ty,
                                is_present,
                                |b, ctx| {
                                    if !absence(ctx, target_ty)
                                        .is_some_and(|a| a.admits_undefined())
                                        && absence(ctx, loaded_ty)
                                            .is_some_and(|a| a.admits_undefined())
                                    {
                                        let is_undef = b
                                            .emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, loaded));
                                        b.choose(
                                            ctx,
                                            target_ty,
                                            is_undef,
                                            |b, ctx| b.fit(ctx, prev, target_ty),
                                            |b, ctx| {
                                                let def = b.strip_undefined(ctx, loaded)?;
                                                b.fit(ctx, def, target_ty)
                                            },
                                        )
                                    } else {
                                        b.fit(ctx, loaded, target_ty)
                                    }
                                },
                                |b, ctx| b.fit(ctx, prev, target_ty),
                            )?;
                        } else {
                            let loaded = match expected_field_ty {
                                Some(target_ty) => self.fit(ctx, loaded, target_ty)?,
                                None => loaded,
                            };
                            *existing = loaded;
                        }
                    } else {
                        let loaded = match expected_field_ty {
                            Some(target_ty) => self.fit(ctx, loaded, target_ty)?,
                            None => loaded,
                        };
                        let loaded_ty = loaded.get_type(ctx);
                        let val = if can_omit
                            && absence(ctx, loaded_ty).is_some_and(|a| a.admits_undefined())
                        {
                            let op = FieldPresentOp::new(ctx, src, field_name.clone())?;
                            let is_present = self.emit_value(ctx, |_| op);
                            self.choose(
                                ctx,
                                loaded_ty,
                                is_present,
                                |_, _| Ok(loaded),
                                |b, ctx| {
                                    Ok(b.emit_value(ctx, |ctx| {
                                        OmittedOp::with_type(ctx, loaded_ty)
                                    }))
                                },
                            )?
                        } else {
                            loaded
                        };
                        field_values.push((field_name, val));
                    }
                }
            }
        }
        let matching_expected = expected_record.filter(|record| {
            record.deref(ctx).fields().is_ok_and(|fields| {
                field_values
                    .iter()
                    .all(|(name, _)| fields.iter().any(|(f, _)| f == name))
                    && fields.iter().all(|(name, ty)| {
                        field_values.iter().any(|(n, _)| n == name)
                            || absence(ctx, *ty).is_some_and(|states| states.admits_undefined())
                    })
            })
        });
        let (record, missing) = match matching_expected {
            Some(record) => {
                let missing: Vec<(usize, TypeHandle)> = record
                    .deref(ctx)
                    .fields()
                    .map_err(|_| anyhow!("record without fields"))?
                    .iter()
                    .enumerate()
                    .filter(|(_, (name, _))| !field_values.iter().any(|(n, _)| n == name))
                    .map(|(index, (_, ty))| (index, *ty))
                    .collect();
                (record, missing)
            }
            None => {
                let fields = field_values
                    .iter()
                    .map(|(name, value)| (name.clone(), value.get_type(ctx)))
                    .collect();
                (self.shapes.mir_record(ctx, fields)?, Vec::new())
            }
        };
        let mut positioned = Vec::with_capacity(field_values.len() + missing.len());
        for (index, ty) in missing {
            positioned.push((
                index,
                self.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, ty)),
            ));
        }
        for (name, value) in field_values {
            let (index, field_ty) = record
                .deref(ctx)
                .field(&name)
                .with_context(|| format!("record has no field {name}"))?;
            let fitted = self.fit(ctx, value, field_ty)?;
            positioned.push((index, fitted));
        }
        positioned.sort_by_key(|(index, _)| *index);
        let operands = positioned.into_iter().map(|(_, value)| value).collect();
        let alloc = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, record, operands);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(Vec::new()));
            alloc
        });
        for step in residue_steps {
            match step {
                ResidueStep::Spread(src) => {
                    self.intrinsic_void(ctx, Intrinsic::JsonSpread, vec![alloc, src]);
                }
                ResidueStep::Insert(key) => {
                    let key_val = self.string(ctx, &key);
                    self.intrinsic_void(ctx, Intrinsic::JsonInsert, vec![alloc, key_val]);
                }
            }
        }
        Ok(alloc)
    }

    /// `const { a, ...rest } = obj`: allocate a record with the remaining fields of `obj`.
    pub(super) fn object_rest(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        exclude_keys: &[String],
        expected: Option<TypeHandle>,
    ) -> Result<Value> {
        let src = self.expression(ctx, object)?;
        let src = self.defined(ctx, src)?;
        let src_rec = TypedHandle::<RecordType>::from_handle(src.get_type(ctx), ctx)
            .context("object rest destructuring requires a typed record")?;
        let expected_record = expected
            .map(|ty| optional_value(ctx, ty).unwrap_or(ty))
            .and_then(|ty| TypedHandle::<RecordType>::from_handle(ty, ctx).ok());
        let src_fields: Vec<(String, TypeHandle)> = src_rec
            .deref(ctx)
            .fields()
            .map_err(|_| anyhow!("record without fields"))?
            .iter()
            .filter(|(name, _)| !exclude_keys.contains(name))
            .cloned()
            .collect();
        let mut field_values: Vec<(String, Value)> = Vec::with_capacity(src_fields.len());
        for (field_name, src_field_ty) in src_fields {
            let can_omit = absence(ctx, src_field_ty).is_some_and(|a| a.admits_undefined());
            let op = FieldGetOp::new(ctx, src, field_name.clone())?;
            let loaded = self.field_read(ctx, &op)?;
            let loaded = match expected_record
                .and_then(|rec| rec.deref(ctx).field(&field_name).map(|(_, ty)| ty))
            {
                Some(target_ty) => self.fit(ctx, loaded, target_ty)?,
                None => loaded,
            };
            let loaded_ty = loaded.get_type(ctx);
            let val = if can_omit && absence(ctx, loaded_ty).is_some_and(|a| a.admits_undefined()) {
                let op = FieldPresentOp::new(ctx, src, field_name.clone())?;
                let is_present = self.emit_value(ctx, |_| op);
                self.choose(
                    ctx,
                    loaded_ty,
                    is_present,
                    |_, _| Ok(loaded),
                    |b, ctx| Ok(b.emit_value(ctx, |ctx| OmittedOp::with_type(ctx, loaded_ty))),
                )?
            } else {
                loaded
            };
            field_values.push((field_name, val));
        }
        let record = match expected_record {
            Some(rec) => rec,
            None => {
                let fields = field_values
                    .iter()
                    .map(|(name, value)| (name.clone(), value.get_type(ctx)))
                    .collect();
                self.shapes.mir_record(ctx, fields)?
            }
        };
        let mut positioned = Vec::with_capacity(field_values.len());
        for (name, value) in field_values {
            let (index, field_ty) = record
                .deref(ctx)
                .field(&name)
                .with_context(|| format!("record has no field {name}"))?;
            let fitted = self.fit(ctx, value, field_ty)?;
            positioned.push((index, fitted));
        }
        positioned.sort_by_key(|(index, _)| *index);
        let operands = positioned.into_iter().map(|(_, value)| value).collect();
        let alloc = self.emit_value(ctx, |ctx| {
            let alloc = AllocOp::new(ctx, record, operands);
            alloc.set_attr_perry_field_order(ctx, perry_mir::FieldOrder(Vec::new()));
            alloc
        });
        self.intrinsic_void(ctx, Intrinsic::JsonSpread, vec![alloc, src]);
        for key in exclude_keys {
            let key_val = self.string(ctx, key);
            self.intrinsic_void(ctx, Intrinsic::JsonExclude, vec![alloc, key_val]);
        }
        Ok(alloc)
    }
}
