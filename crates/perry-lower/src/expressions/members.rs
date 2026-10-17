//! Member access: `object.property`, `object[index]` and `++object.field`.
use super::*;
use crate::collections::{
    dict_entry_types, is_collection_or_view_record, map_box_record, map_entry_types,
    set_box_record, set_entry_type,
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
            let (id, _) = self
                .shapes
                .static_field(class_name, property)
                .with_context(|| format!("static field {class_name}.{property} not found"))?;
            let update_op = match op {
                BinaryOp::Add => UpdateOp::Increment,
                BinaryOp::Sub => UpdateOp::Decrement,
                _ => bail!("invalid property update"),
            };
            return self.update(ctx, id, &update_op, prefix);
        }
        let object = self.expression(ctx, object)?;
        let object = self.defined(ctx, object)?;
        if let Some((class_name, type_args)) = self.shapes.class_of_type(ctx, object.get_type(ctx))
            && (self.shapes.class_getter(class_name, property).is_some()
                || self.shapes.class_setter(class_name, property).is_some())
        {
            let previous = if let Some((owner_class, getter)) =
                self.shapes.class_getter(class_name, property)
            {
                self.class_getter_call(ctx, object, owner_class, &type_args, getter)?
            } else {
                let get = FieldGetOp::new(ctx, object, property.to_string())
                    .map_err(|_| anyhow!("property update needs a declared numeric field"))?;
                self.append_value(ctx, &get)
            };
            let field_type = previous.get_type(ctx);
            let old = self.arithmetic(ctx, previous);
            ensure!(
                kind(ctx, old)? == Kind::Number,
                "property update needs a numeric field"
            );
            let one = self.number(ctx, 1.0);
            let kind = match op {
                BinaryOp::Add => BinaryKind::Add,
                BinaryOp::Sub => BinaryKind::Sub,
                _ => bail!("invalid property update"),
            };
            let next = self.binary(ctx, kind, old, one);
            let stored = self.fit(ctx, next, field_type)?;
            if let Some((owner_class, setter)) = self.shapes.class_setter(class_name, property) {
                self.class_setter_call_value(ctx, object, owner_class, &type_args, setter, stored)?;
            } else {
                self.emit(ctx, |ctx| {
                    FieldSetOp::new(ctx, object, property.to_string(), stored)
                });
            }
            return Ok(if prefix { next } else { old });
        }
        let get = FieldGetOp::new(ctx, object, property.to_string())
            .map_err(|_| anyhow!("property update needs a declared numeric field"))?;
        let previous = self.append_value(ctx, &get);
        let field_type = previous.get_type(ctx);
        let old = self.arithmetic(ctx, previous);
        ensure!(
            kind(ctx, old)? == Kind::Number,
            "property update needs a numeric field"
        );
        let one = self.number(ctx, 1.0);
        let kind = match op {
            BinaryOp::Add => BinaryKind::Add,
            BinaryOp::Sub => BinaryKind::Sub,
            _ => bail!("invalid property update"),
        };
        let next = self.binary(ctx, kind, old, one);
        let stored = self.fit(ctx, next, field_type)?;
        self.emit(ctx, |ctx| {
            FieldSetOp::new(ctx, object, property.to_string(), stored)
        });
        Ok(if prefix { next } else { old })
    }

    /// `object.property`, by the kind of `object`.
    pub(super) fn property_get(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
    ) -> Result<Value> {
        if let Expr::ClassRef(class_name) = object {
            return self.class_static_field_get(ctx, class_name, property);
        }
        let value = self.expression(ctx, object)?;
        // Property access on `undefined` throws TypeError, in ECMAScript and here.
        let value = self.defined(ctx, value)?;
        let ty = value.get_type(ctx);
        Ok(match (kind(ctx, value)?, property) {
            (Kind::Url, field) => self.url_field(ctx, value, field)?,
            (Kind::Decoder, "encoding") => self.string(ctx, "utf-8"),
            (Kind::Decoder, "fatal" | "ignoreBOM") => {
                let intrinsic = if property == "fatal" {
                    Intrinsic::DecoderFatal
                } else {
                    Intrinsic::DecoderIgnoreBom
                };
                self.intrinsic(ctx, intrinsic, vec![value])
            }
            (Kind::String, "length") => {
                let length = self.emit_value(ctx, |ctx| StringLenOp::new(ctx, value));
                self.emit_value(ctx, |ctx| {
                    NumericCastOp::new(ctx, length, FP64Type::get(ctx).into())
                })
            }
            (Kind::Array, "length") => self.array_length(ctx, value),
            (Kind::Array, "size") => self.collection_size(ctx, value)?,
            (Kind::Array, field) if let Some((k_ty, v_ty, _)) = dict_entry_types(ctx, ty) => {
                let key = self.string(ctx, field);
                self.kv_get(ctx, value, key, k_ty, v_ty)?
            }
            (Kind::Bytes, "buffer") => self.intrinsic(ctx, Intrinsic::BytesBuffer, vec![value]),
            (Kind::Bytes, "byteOffset") => self.intrinsic(ctx, Intrinsic::BytesOffset, vec![value]),
            (Kind::ByteBuffer, "byteLength") => {
                self.intrinsic(ctx, Intrinsic::BufferLength, vec![value])
            }
            (Kind::Bytes, "length" | "byteLength") => {
                self.intrinsic(ctx, Intrinsic::BytesLength, vec![value])
            }
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
                    other => bail!("settled results have no property {other}"),
                };
                let op =
                    SettledFieldOp::new(ctx, field, value).context("expected a settled result")?;
                self.append_value(ctx, &op)
            }
            (
                Kind::Headers
                | Kind::Response
                | Kind::Request
                | Kind::Writable
                | Kind::Writer
                | Kind::Readable
                | Kind::Reader
                | Kind::ByobReader
                | Kind::ReadResult,
                name,
            ) => self.web_member(ctx, value, name, None)?,
            (Kind::Instant, "epochMilliseconds") => {
                self.intrinsic(ctx, Intrinsic::InstantMs, vec![value])
            }
            (Kind::PlainDate | Kind::PlainDateTime, field) => {
                self.temporal_field(ctx, value, field)?
            }
            (Kind::Abort, "signal") => value,
            (Kind::Abort, "aborted") => self.intrinsic(ctx, Intrinsic::AbortAborted, vec![value]),
            (Kind::Stats, "size") => self.intrinsic(ctx, Intrinsic::StatsSize, vec![value]),
            (Kind::Exception, "cause") => {
                self.intrinsic(ctx, Intrinsic::ExceptionCause, vec![value])
            }
            (Kind::Exception, "name") => self.intrinsic(ctx, Intrinsic::ExceptionName, vec![value]),
            (Kind::Exception, "message") => {
                self.emit_value(ctx, |ctx| ExceptionMessageOp::new(ctx, value))
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
                if let Some((class_name, type_args)) = self.shapes.class_of_type(ctx, ty)
                    && let Some((owner_class, getter)) =
                        self.shapes.class_getter(class_name, field) =>
            {
                self.class_getter_call(ctx, value, owner_class, &type_args, getter)?
            }
            (Kind::Record, field) => match FieldGetOp::new(ctx, value, field.to_string()) {
                Ok(op) => self.field_read(ctx, &op)?,
                Err(_) if self.rest_excluded_keys.iter().any(|k| k == field) => {
                    self.absent_literal(ctx, false, None)?
                }
                Err(_) => bail!("property {field} does not exist on this shape (subset rule R1)"),
            },
            (receiver_kind, property) => {
                bail!(
                    "property {property} is not supported on {} yet",
                    receiver_kind.label()
                )
            }
        })
    }

    /// The value of a record field read: `undefined` normalized, 64-bit integers as bigint.
    pub(crate) fn field_read(&mut self, ctx: &mut Context, op: &FieldGetOp) -> Result<Value> {
        let loaded = self.append_value(ctx, op);
        let loaded = self.normalize_optional_read(ctx, loaded)?;
        Ok(
            if loaded
                .get_type(ctx)
                .deref(ctx)
                .downcast_ref::<IntegerType>()
                .is_some_and(|int| int.width() == 64)
            {
                let bigint = BigIntType::get(ctx).into();
                self.emit_value(ctx, |ctx| NumericCastOp::new(ctx, loaded, bigint))
            } else {
                loaded
            },
        )
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
                    _ => bail!("JSON lookup requires a string or number key"),
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
                        let Ok(rec) = TypedHandle::<RecordType>::from_handle(ty, ctx) else {
                            bail!("expected a record for dynamic index");
                        };
                        let key = self.string_operand(ctx, index)?;
                        return self.record_dynamic_get(ctx, array, key, rec);
                    }
                };
                let op = FieldGetOp::new(ctx, array, field.clone())
                    .map_err(|_| anyhow!("record has no field {field}"))?;
                self.field_read(ctx, &op)
            }
            Kind::Array => {
                if let Some((k_ty, v_ty, _)) = dict_entry_types(ctx, ty) {
                    let key = self.expression_with(ctx, index, Some(k_ty))?;
                    return self.kv_get(ctx, array, key, k_ty, v_ty);
                }
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index)?;
                let val = self.append_value(ctx, &op);
                if set_entry_type(ctx, ty).is_some() {
                    let entry = self.defined(ctx, val)?;
                    let get_op = FieldGetOp::new(ctx, entry, "_0".to_string())?;
                    return Ok(self.append_value(ctx, &get_op));
                }
                if let Some((k_ty, v_ty, _)) = map_entry_types(ctx, ty) {
                    let entry = self.defined(ctx, val)?;
                    let (k, v) = self.entry_fields(ctx, entry)?;
                    let tuple_rec = self.shapes.pair_type(ctx, k_ty, v_ty)?;
                    return Ok(self.emit_value(ctx, |ctx| AllocOp::new(ctx, tuple_rec, vec![k, v])));
                }
                Ok(val)
            }
            _ => bail!("indexing is only supported on arrays so far"),
        }
    }
}

/// The arity of a tuple record type; `None` for other types.
fn tuple_length(ctx: &Context, ty: TypeHandle) -> Result<Option<usize>> {
    match TypedHandle::<RecordType>::from_handle(ty, ctx) {
        Ok(record) if record.deref(ctx).is_tuple() => Ok(Some(
            record
                .deref(ctx)
                .fields()
                .map_err(|_| anyhow!("tuple without fields"))?
                .len(),
        )),
        _ => Ok(None),
    }
}
