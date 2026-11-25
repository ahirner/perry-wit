//! Member access: `object.property`, `object[index]` and `++object.field`.
use super::*;
use crate::lower::collections::{
    dict_entry_types, is_collection_or_view_record, map_box_record, set_box_record,
};

impl<'s> FunctionBuilder<'s> {
    /// `++object.field` and its relatives, on a numeric record field.
    pub(super) fn property_update(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
        op: &BinaryOp,
        prefix: bool,
    ) -> Result<Value> {
        if let Expr::ClassRef(class_name) = object {
            let id = self
                .shapes
                .static_field(class_name, property)
                .ok_or_else(|| {
                    diag!(
                        codes::TYPE_MISMATCH,
                        "static field {class_name}.{property} not found"
                    )
                })?;
            let update_op = match op {
                BinaryOp::Add => UpdateOp::Increment,
                BinaryOp::Sub => UpdateOp::Decrement,
                _ => bail_internal!("invalid property update"),
            };
            return self.update(ctx, id, &update_op, prefix);
        }
        let object = self.expression(ctx, object)?;
        let object = self.defined(ctx, object)?;
        let class_info = self.shapes.class_of_type(ctx, object.get_type(ctx));
        let previous = if let Some(class_name) = class_info
            && let Some((owner_class, getter)) = self.shapes.class_getter(class_name, property)
        {
            self.class_getter_call(ctx, object, owner_class, getter)?
        } else {
            let get = FieldGetOp::new(ctx, object, property.to_string()).map_err(|_| {
                Diagnostic::new(
                    codes::TYPE_MISMATCH,
                    "property update needs a declared numeric field",
                )
            })?;
            self.append_value(ctx, &get)
        };
        let field_type = previous.get_type(ctx);
        let old = self.arithmetic(ctx, previous);
        ensure_diag!(
            kind(ctx, old)? == Kind::Number,
            codes::TYPE_MISMATCH,
            "property update needs a numeric field"
        );
        let one = self.number(ctx, 1.0);
        let kind = match op {
            BinaryOp::Add => BinaryKind::Add,
            BinaryOp::Sub => BinaryKind::Sub,
            _ => bail_internal!("invalid property update"),
        };
        let next = self.binary(ctx, kind, old, one);
        let stored = self.fit(ctx, next, field_type)?;
        if let Some(class_name) = class_info
            && let Some((owner_class, setter)) = self.shapes.class_setter(class_name, property)
        {
            self.class_setter_call_value(ctx, object, owner_class, setter, stored)?;
        } else {
            self.emit(ctx, |ctx| {
                FieldSetOp::new(ctx, object, property.to_string(), stored)
            });
        }
        Ok(if prefix { next } else { old })
    }

    pub(crate) fn string_length(&mut self, ctx: &mut Context, string: Value) -> Value {
        let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, string));
        self.emit_value(ctx, |ctx| {
            NumericCastOp::new(ctx, length, FP64Type::get(ctx).into())
        })
    }

    /// `object.property`, by the kind of `object`.
    pub(super) fn property_get(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
        byte_offset: u32,
    ) -> Result<Value> {
        if let Expr::ClassRef(class_name) = object {
            return self.class_static_field_get(ctx, class_name, property);
        }
        let value = self.expression(ctx, object)?;
        let value = self.receiver(ctx, value, byte_offset)?;
        let ty = value.get_type(ctx);
        if crate::lower::value_unions::is_value_union(ctx, ty) {
            return self.value_union_member(ctx, value, property);
        }
        Ok(match (kind(ctx, value)?, property) {
            (Kind::Url, field) => self.url_field(ctx, value, field)?,
            (Kind::String, "length") => self.string_length(ctx, value),
            (Kind::Array, field) if let Some((k_ty, v_ty, _)) = dict_entry_types(ctx, ty) => {
                let key = self.string(ctx, field);
                self.kv_get(ctx, value, key, k_ty, v_ty)?
            }
            (Kind::Array, "groups" | "index" | "input") => {
                ensure_unsupported!(
                    Self::regex_result(ctx, value),
                    "regex result metadata requires an exec result or a non-global match result"
                );
                let intrinsic = match property {
                    "groups" => Intrinsic::RegexGroups,
                    "index" => Intrinsic::RegexIndex,
                    _ => Intrinsic::RegexInput,
                };
                self.intrinsic(ctx, intrinsic, vec![value])
            }
            (Kind::Array, "length") => self.array_length(ctx, value),
            (Kind::Array, "size") => self.collection_size(ctx, value)?,
            (Kind::Json, "length") => self.intrinsic(ctx, Intrinsic::JsonLength, vec![value]),
            (Kind::Json, name) => {
                let name = self.string(ctx, name);
                self.intrinsic(ctx, Intrinsic::JsonMember, vec![value, name])
            }
            (Kind::Settled, field) => {
                let field = match field {
                    "status" => SettledField::Status,
                    "value" => SettledField::Value,
                    "reason" => SettledField::Reason,
                    other => bail_diag!(
                        codes::TYPE_MISMATCH,
                        "settled results have no property {other}"
                    ),
                };
                let op = SettledFieldOp::new(ctx, field, value)
                    .ok_or_else(|| Diagnostic::internal("expected a settled result"))?;
                self.append_value(ctx, &op)
            }
            (Kind::Variant, field) => self.variant_member(ctx, object, value, field)?,
            (Kind::Record, "size")
                if map_box_record(ctx, ty).is_some() || set_box_record(ctx, ty).is_some() =>
            {
                self.collection_size(ctx, value)?
            }
            (Kind::Record, "length") if is_collection_or_view_record(ctx, ty) => {
                self.collection_backing_length(ctx, value)?
            }
            (Kind::Record, "length") if let Some(len) = tuple_length(ctx, ty)? => {
                self.number(ctx, len as f64)
            }
            (Kind::Record, field)
                if let Some(class_name) = self.shapes.class_of_type(ctx, ty)
                    && let Some((owner_class, getter)) =
                        self.shapes.class_getter(class_name, field) =>
            {
                self.class_getter_call(ctx, value, owner_class, getter)?
            }
            (Kind::Record, field) => match FieldGetOp::new(ctx, value, field.to_string()) {
                Ok(op) => self.field_read(ctx, &op)?,
                Err(_) if self.rest_excluded_keys.iter().any(|k| k == field) => {
                    self.absent_literal(ctx, false, None)?
                }
                Err(_) => bail_diag!(
                    codes::TYPE_MISMATCH,
                    "property {field} does not exist on this shape (subset rule R1)"
                ),
            },
            (receiver_kind, property) => {
                if let Some(call) = crate::lower::methods::Call::member(
                    crate::lower::methods::Receiver::Value(receiver_kind),
                    property,
                    None,
                )? {
                    call.lower(self, ctx, value)?.ok_or_else(|| {
                        Diagnostic::new(codes::TYPE_MISMATCH, "properties produce a value")
                    })?
                } else {
                    bail_unsupported!(
                        "property {property} is not supported on {} yet",
                        receiver_kind.label()
                    )
                }
            }
        })
    }

    /// The value of a record field read: `undefined` normalized, 64-bit integers as bigint.
    pub(crate) fn field_read(&mut self, ctx: &mut Context, op: &FieldGetOp) -> Result<Value> {
        let loaded = self.append_value(ctx, op);
        let loaded = self.normalize_optional_read(ctx, loaded)?;
        Ok(self.bigint_view(ctx, loaded))
    }

    /// `object[index]` on arrays, strings, bytes, JSON values and records (literal keys).
    pub(super) fn index_get(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        index: &'s Expr,
    ) -> Result<Value> {
        let array = self.expression(ctx, object)?;
        let array = self.defined(ctx, array)?;
        let ty = array.get_type(ctx);
        match kind(ctx, array)? {
            Kind::String => {
                let index = self.number_operand(ctx, index)?;
                Ok(self.intrinsic(ctx, Intrinsic::StringIndex, vec![array, index]))
            }
            Kind::Bytes => {
                let index = self.number_operand(ctx, index)?;
                Ok(self.intrinsic(ctx, Intrinsic::BytesGet, vec![array, index]))
            }
            Kind::Json => {
                let index = self.expression(ctx, index)?;
                let index = self.defined(ctx, index)?;
                let intrinsic = match kind(ctx, index)? {
                    Kind::String => Intrinsic::JsonMember,
                    Kind::Number => Intrinsic::JsonIndex,
                    _ => bail_diag!(
                        codes::TYPE_MISMATCH,
                        "JSON lookup requires a string or number key"
                    ),
                };
                Ok(self.intrinsic(ctx, intrinsic, vec![array, index]))
            }
            Kind::Record if is_collection_or_view_record(ctx, ty) => {
                let idx = self.number_operand(ctx, index)?;
                self.collection_view_element_at(ctx, array, idx)
            }
            Kind::Record => {
                let field = match (index, self.iteration.index(index)) {
                    (Expr::String(name), _) => name.clone(),
                    (Expr::Number(n), _) if n.fract() == 0.0 && *n >= 0.0 => {
                        format!("_{}", *n as u64)
                    }
                    (Expr::Integer(n), _) if *n >= 0 => format!("_{n}"),
                    (_, Some(position)) => format!("_{position}"),
                    _ => {
                        let Some(rec) = perry_mir::typed::<RecordType>(ctx, ty) else {
                            bail_internal!("expected a record for dynamic index");
                        };
                        let key = self.string_operand(ctx, index)?;
                        return self.record_dynamic_get(ctx, array, key, rec);
                    }
                };
                let op = FieldGetOp::new(ctx, array, field.clone())
                    .map_err(|_| diag!(codes::TYPE_MISMATCH, "record has no field {field}"))?;
                self.field_read(ctx, &op)
            }
            Kind::Array => {
                if let Some((k_ty, v_ty, _)) = dict_entry_types(ctx, ty) {
                    let key = self.expression_with(ctx, index, Some(k_ty))?;
                    return self.kv_get(ctx, array, key, k_ty, v_ty);
                }
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index).to_diag(ctx)?;
                let val = self.append_value(ctx, &op);
                Ok(val)
            }
            _ => bail_unsupported!("indexing is only supported on arrays so far"),
        }
    }
}

/// Return the arity of a tuple record type, or `None` for other types.
fn tuple_length(ctx: &Context, ty: TypeHandle) -> Result<Option<usize>> {
    match perry_mir::typed::<RecordType>(ctx, ty) {
        Some(record) if record.deref(ctx).kind() == RecordKind::Tuple => Ok(Some(
            record
                .deref(ctx)
                .fields()
                .map_err(|_| Diagnostic::internal("tuple without fields"))?
                .len(),
        )),
        _ => Ok(None),
    }
}
