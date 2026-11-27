//! Member access: `object.property`, `object[index]` and `++object.field`.
use super::*;
use crate::lower::collections::dict_entry_types;

#[derive(Clone, Copy)]
pub(in crate::lower) enum Member<'p> {
    Field(Value, &'p str, usize, TypeHandle),
    Accessor(Value, FuncId),
}

pub(in crate::lower) enum Assigned<'s> {
    Expression(&'s Expr),
    Value(Value),
}

impl<'s> Assigned<'s> {
    fn lower(
        self,
        b: &mut FunctionBuilder<'s, '_>,
        ctx: &mut Context,
        ty: TypeHandle,
    ) -> Result<Value> {
        match self {
            Self::Expression(expr) => b.expression_with(ctx, expr, Some(ty)),
            Self::Value(value) => Ok(value),
        }
    }
}

impl<'p> Member<'p> {
    pub(in crate::lower) fn field(
        ctx: &Context,
        object: Value,
        name: &'p str,
        kind: Option<RecordKind>,
    ) -> Result<Self> {
        let record = perry_mir::typed::<RecordType>(ctx, object.get_type(ctx))
            .filter(|record| kind.is_none_or(|kind| record.deref(ctx).kind() == kind))
            .ok_or_else(|| unsupported!("property access requires a record"))?;
        let (index, ty) = record.deref(ctx).field(name).ok_or_else(|| {
            diag!(
                codes::TYPE_MISMATCH,
                "property {name} does not exist on this shape (subset rule R1)"
            )
        })?;
        Ok(Self::Field(object, name, index, ty))
    }

    pub(in crate::lower) fn read(
        self,
        b: &mut FunctionBuilder,
        ctx: &mut Context,
    ) -> Result<Value> {
        match self {
            Self::Field(object, name, _, ty) => {
                let get = FieldGetOp::with_type(ctx, object, name.to_owned(), ty).to_diag(ctx)?;
                b.field_read(ctx, get)
            }
            Self::Accessor(object, get) => b
                .call_values(ctx, get, std::iter::once(object))?
                .ok_or_else(|| diag!(codes::TYPE_MISMATCH, "class member produced no value")),
        }
    }

    pub(in crate::lower) fn write<'s>(
        self,
        b: &mut FunctionBuilder<'s, '_>,
        ctx: &mut Context,
        rhs: Assigned<'s>,
    ) -> Result<Option<Value>> {
        match self {
            Self::Accessor(object, set) => {
                let value = if let Some(param) = b.signature(set)?.params.get(1) {
                    Some(rhs.lower(b, ctx, param.base.carrier)?)
                } else {
                    if let Assigned::Expression(expr) = rhs {
                        b.discard_function_operand(ctx, expr)?;
                    }
                    None
                };
                b.call_values(ctx, set, std::iter::once(object).chain(value))?;
                Ok(value)
            }
            Self::Field(object, name, index, ty) => {
                let value = rhs.lower(b, ctx, ty)?;
                let value = b.fit(ctx, value, ty)?;
                let set = if b
                    .shapes
                    .class_records
                    .get(&object.get_type(ctx))
                    .and_then(|class| class.fields().nth(index))
                    .is_some_and(|(field, _)| hir_compat::private_field(field))
                {
                    FieldSetOp::private
                } else {
                    FieldSetOp::new
                };
                b.emit(set(ctx, object, name.to_owned(), value), ctx);
                Ok(Some(b.bigint_view(ctx, value)))
            }
        }
    }
}

impl<'s> FunctionBuilder<'s, '_> {
    /// `++object.field` and its relatives, on a numeric record field.
    pub(super) fn property_update(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        property: &str,
        op: &BinaryOp,
        prefix: bool,
        (get, set): (Option<FuncId>, Option<FuncId>),
    ) -> Result<Value> {
        if let Expr::ClassRef(class_name) = object {
            let id = self.shapes.static_field(class_name, property)?;
            return self.update(ctx, id, op, prefix);
        }
        let object = self.expression(ctx, object)?;
        let object = self.defined(ctx, object)?;
        let (read, write) = match (get, set) {
            (None, None) => {
                let field = Member::field(ctx, object, property, None)?;
                (field, field)
            }
            (Some(get), Some(set)) => {
                (Member::Accessor(object, get), Member::Accessor(object, set))
            }
            _ => bail_diag!(
                codes::TYPE_MISMATCH,
                "updating {property} requires both a getter and a setter"
            ),
        };
        let previous = read.read(self, ctx)?;
        let (old, next) = self.numeric_update(ctx, previous, op)?;
        write.write(self, ctx, Assigned::Value(next))?;
        Ok(if prefix { next } else { old })
    }

    pub(crate) fn string_length(&mut self, ctx: &mut Context, string: Value) -> Value {
        let length = self.emit_value(StringLenOp::new(ctx, string), ctx);
        self.emit_value(
            NumericCastOp::new(ctx, length, FP64Type::get(ctx).into()),
            ctx,
        )
    }

    /// `object.property`, by the kind of `object`.
    pub(in crate::lower) fn read_member(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        value: Value,
        key: hir_compat::MemberKey<'s>,
        getter: Option<FuncId>,
    ) -> Result<Option<Value>> {
        let result = if let Some(getter) = getter {
            let receiver = self.defined(ctx, value)?;
            self.call_values(ctx, getter, std::iter::once(receiver))
        } else {
            match key {
                hir_compat::MemberKey::Named(name, offset) => {
                    self.property_of_value(ctx, object, value, name, offset)
                }
                hir_compat::MemberKey::Computed(index, _) => self.index_get(ctx, value, index),
            }
            .map(Some)
        };
        result.map_err(|e| self.shapes.annotate_member(e, key.offset()))
    }

    fn property_of_value(
        &mut self,
        ctx: &mut Context,
        object: &'s Expr,
        value: Value,
        property: &str,
        byte_offset: u32,
    ) -> Result<Value> {
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
                let intrinsic = match property {
                    "groups" => Intrinsic::RegexGroups,
                    "index" => Intrinsic::RegexIndex,
                    _ => Intrinsic::RegexInput,
                };
                let span = self.shapes.locations.member_span(byte_offset);
                self.located_intrinsic(ctx, intrinsic, vec![value], span)
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
                self.emit_value(op, ctx)
            }
            (Kind::Variant, field) => self.variant_member(ctx, object, value, field)?,
            (Kind::Record, "size") if self.shapes.collection_box(ctx, ty).is_some() => {
                self.collection_size(ctx, value)?
            }
            (Kind::Record, "length") if self.shapes.collection(ctx, ty).is_some() => {
                self.collection_backing_length(ctx, value)?
            }
            (Kind::Record, "length")
                if let Some(record) = perry_mir::typed::<RecordType>(ctx, ty)
                    && record.deref(ctx).kind() == RecordKind::Tuple =>
            {
                let len = record.deref(ctx).fields().to_diag(ctx)?.len();
                self.number(ctx, len as f64)
            }
            (Kind::Record, field) => match Member::field(ctx, value, field, None) {
                Ok(member) => member.read(self, ctx)?,
                Err(_) if self.frame.rest_excluded_keys.iter().any(|k| k == field) => {
                    self.absent_literal(ctx, false, None)?
                }
                Err(error) => return Err(error),
            },
            (receiver_kind, property) => {
                if let Some(call) = crate::lower::methods::Call::member(
                    crate::lower::methods::Receiver::Value(receiver_kind),
                    property,
                    None,
                )? {
                    call.lower(self, ctx, value, Some(byte_offset))?
                        .ok_or_else(|| {
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
    pub(crate) fn field_read(&mut self, ctx: &mut Context, op: FieldGetOp) -> Result<Value> {
        let loaded = self.emit_value(op, ctx);
        let loaded = self.normalize_optional_read(ctx, loaded)?;
        Ok(self.bigint_view(ctx, loaded))
    }

    /// `object[index]` on arrays, strings, bytes, JSON values and records (literal keys).
    fn index_get(&mut self, ctx: &mut Context, array: Value, index: &'s Expr) -> Result<Value> {
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
            Kind::Record if self.shapes.collection(ctx, ty).is_some() => {
                let idx = self.number_operand(ctx, index)?;
                self.collection_view_element_at(ctx, array, idx)
            }
            Kind::Record => {
                let field = match (
                    index,
                    hir_compat::tuple_position(index).or_else(|| self.frame.cursors.index(index)),
                ) {
                    (Expr::String(name), _) => name.clone(),
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
                self.field_read(ctx, op)
            }
            Kind::Array => {
                if let Some((k_ty, v_ty, _)) = dict_entry_types(ctx, ty) {
                    let key = self.expression_with(ctx, index, Some(k_ty))?;
                    return self.kv_get(ctx, array, key, k_ty, v_ty);
                }
                let index = self.index(ctx, index)?;
                let op = ArrayGetOp::new(ctx, array, index).to_diag(ctx)?;
                let val = self.emit_value(op, ctx);
                Ok(val)
            }
            _ => bail_unsupported!("indexing is only supported on arrays so far"),
        }
    }
}
