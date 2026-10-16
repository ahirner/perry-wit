//! Numeric layout specialization of aggregate types from converged slot domains.
use super::*;
use pliron::identifier::Legaliser;

impl ProgramFacts {
    /// The specialized type of `value` in context `id`, if its layout or domain changes.
    pub(in crate::facts) fn specialized_value_type(
        &self,
        ctx: &Context,
        id: usize,
        value: Value,
    ) -> Option<TypeHandle> {
        let ty = value.get_type(ctx);
        if aggregate(ctx, ty) && !ty.deref(ctx).is::<PromiseType>() {
            if let Some(&cell) = self.states[id].heap_values.get(&value)
                && let Some(spec) = self.specialized_cell_type(ctx, ty, cell)
            {
                return Some(spec);
            }
        }
        let state = &self.states[id];
        if let Some(op) = value.defining_op() {
            if let Some(fg) = Operation::get_op::<FieldGetOp>(op, ctx) {
                let base = op.deref(ctx).get_operand(0);
                let base_spec = self.specialized_value_type(ctx, id, base)?;
                let field_name = fg.field(ctx);
                let field_ty = base_spec
                    .deref(ctx)
                    .downcast_ref::<RecordType>()?
                    .fields()
                    .ok()?
                    .iter()
                    .find(|(n, _)| *n == field_name)?
                    .1;
                return (field_ty != ty).then_some(field_ty);
            }
            if Operation::get_op::<ArrayAtOp>(op, ctx).is_some() {
                let base = op.deref(ctx).get_operand(0);
                let base_spec = self.specialized_value_type(ctx, id, base)?;
                let elem_ty = base_spec.deref(ctx).downcast_ref::<ArrayType>()?.element();
                return (elem_ty != ty).then_some(elem_ty);
            }
            if Operation::get_op::<ArrayGetOp>(op, ctx).is_some() {
                let base = op.deref(ctx).get_operand(0);
                let base_spec = self.specialized_value_type(ctx, id, base)?;
                let elem_ty = base_spec.deref(ctx).downcast_ref::<ArrayType>()?.element();
                let opt_ty: TypeHandle = crate::types::OptionalType::get(ctx, elem_ty).into();
                return (opt_ty != ty).then_some(opt_ty);
            }
            if Operation::get_op::<UnwrapOp>(op, ctx).is_some() {
                let base = op.deref(ctx).get_operand(0);
                let base_spec = self.specialized_value_type(ctx, id, base)?;
                let inner_ty = crate::optional::optional_value(ctx, base_spec)?;
                return (inner_ty != ty).then_some(inner_ty);
            }
            if Operation::get_op::<WrapOp>(op, ctx).is_some() {
                let base = op.deref(ctx).get_operand(0);
                let base_spec = self.specialized_value_type(ctx, id, base)?;
                let absence = ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::OptionalType>()?
                    .absence();
                let opt_ty: TypeHandle =
                    crate::types::OptionalType::get_with(ctx, base_spec, absence).into();
                return (opt_ty != ty).then_some(opt_ty);
            }
            if Operation::get_op::<crate::locals::LoadOp>(op, ctx).is_some()
                || Operation::get_op::<AwaitOp>(op, ctx).is_some()
            {
                return None;
            }
            if let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx) {
                let ok = match intrinsic.intrinsic(ctx) {
                    Some(
                        crate::intrinsics::Intrinsic::BigIntNegate
                        | crate::intrinsics::Intrinsic::BytesAt
                        | crate::intrinsics::Intrinsic::ParseInt,
                    ) => true,
                    Some(
                        crate::intrinsics::Intrinsic::MathFloor
                        | crate::intrinsics::Intrinsic::MathCeil
                        | crate::intrinsics::Intrinsic::MathTrunc
                        | crate::intrinsics::Intrinsic::MathRound
                        | crate::intrinsics::Intrinsic::MathAbs,
                    ) => {
                        let arg = op.deref(ctx).get_operand(0);
                        state.representation(arg).is_some() || state.fact(arg) == Fact::Numeric
                    }
                    Some(
                        crate::intrinsics::Intrinsic::MathMin
                        | crate::intrinsics::Intrinsic::MathMax,
                    ) => {
                        let lhs = op.deref(ctx).get_operand(0);
                        let rhs = op.deref(ctx).get_operand(1);
                        (state.representation(lhs).is_some() && state.representation(rhs).is_some())
                            || state.fact(lhs) == Fact::Numeric
                            || state.fact(rhs) == Fact::Numeric
                    }
                    _ => false,
                };
                if !ok {
                    return None;
                }
            }
        }
        if state.fact(value) == Fact::Numeric {
            return numeric_type_for(ctx, ty);
        }
        None
    }

    /// The specialized type of result `pos` of context `id`, declared as `ty`.
    pub(in crate::facts) fn specialized_result_type(
        &self,
        ctx: &Context,
        id: usize,
        pos: usize,
        ty: TypeHandle,
    ) -> Option<TypeHandle> {
        let res = &self.states[id].results[pos];
        if aggregate(ctx, ty) && !ty.deref(ctx).is::<PromiseType>() {
            let cell = res.heap?;
            return self.specialized_cell_type(ctx, ty, cell);
        }
        if res.domain == Fact::Numeric {
            return numeric_type_for(ctx, ty);
        }
        None
    }

    fn specialized_cell_type(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        cell: usize,
    ) -> Option<TypeHandle> {
        let c = &self.heap.cells[cell];
        let mut visiting = HashMap::new();
        let (t, real) = self.specialized_type_inner(ctx, ty, &c.layout_objects, &mut visiting)?;
        real.then_some(t)
    }

    /// Specializes `ty` when every object in `objects` agrees on numeric slots.
    /// Returns `Some((type, real_change))` where `real_change` is false if the result is
    /// merely an unverified recursive reservation stub.
    fn specialized_type_inner(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut HashMap<TypeHandle, TypeHandle>,
    ) -> Option<(TypeHandle, bool)> {
        if let Some(&target) = visiting.get(&ty) {
            return Some((target, false));
        }
        if let Some((inner_ty, absence)) = {
            let deref = ty.deref(ctx);
            deref
                .downcast_ref::<crate::types::OptionalType>()
                .map(|opt| (opt.value(), opt.absence()))
        } {
            let (new_inner, real) =
                self.specialized_type_inner(ctx, inner_ty, objects, visiting)?;
            return Some((
                crate::types::OptionalType::get_with(ctx, new_inner, absence).into(),
                real,
            ));
        }
        if objects.is_empty() {
            return None;
        }
        let array_elem = ty
            .deref(ctx)
            .downcast_ref::<ArrayType>()
            .map(|a| a.element());
        if let Some(elem_ty) = array_elem {
            let mut element_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Array { element, .. } = &self.heap.objects[o].contents else {
                    return None;
                };
                element_slots.push(*element);
            }
            let all_numeric = element_slots
                .iter()
                .all(|&s| self.heap.cells[s].domain == Fact::Numeric);
            if all_numeric && let Some(new_elem) = numeric_type_for(ctx, elem_ty) {
                return Some((ArrayType::get(ctx, new_elem).into(), true));
            }
            if aggregate(ctx, elem_ty) {
                let mut elem_objects = BTreeSet::new();
                for &s in &element_slots {
                    elem_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if let Some((new_elem, real)) =
                    self.specialized_type_inner(ctx, elem_ty, &elem_objects, visiting)
                {
                    return Some((ArrayType::get(ctx, new_elem).into(), real));
                }
            }
            return None;
        }
        // Field types are copied out: instantiating types while the record is borrowed would
        // re-borrow the type storage mutably.
        let (rec_name, is_tuple, fields) = {
            let deref = ty.deref(ctx);
            let rec = deref.downcast_ref::<RecordType>()?;
            (
                rec.name().clone(),
                rec.is_tuple(),
                rec.fields().ok()?.to_vec(),
            )
        };
        let is_rec = is_recursive_type(ctx, ty, &mut HashSet::new());
        let reserved_name = if is_rec {
            let name = Legaliser::default().legalise(&format!("{rec_name}__num_rec"));
            let reserved_handle: TypeHandle = if is_tuple {
                RecordType::get_tuple(ctx, name.clone(), None).ok()?.into()
            } else {
                RecordType::get(ctx, name.clone(), None).ok()?.into()
            };
            visiting.insert(ty, reserved_handle);
            Some(name)
        } else {
            None
        };
        let mut real_change = false;
        let mut spec_fields = Vec::with_capacity(fields.len());
        let mut spec_tag = String::new();
        for (idx, (fname, fty)) in fields.into_iter().enumerate() {
            let mut field_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Record(slots) = &self.heap.objects[o].contents else {
                    if is_rec {
                        visiting.remove(&ty);
                    }
                    return None;
                };
                let Some(&s) = slots.get(idx) else {
                    if is_rec {
                        visiting.remove(&ty);
                    }
                    return None;
                };
                field_slots.push(s);
            }
            let all_numeric = field_slots
                .iter()
                .all(|&s| self.heap.cells[s].domain == Fact::Numeric);
            if all_numeric && let Some(new_fty) = numeric_type_for(ctx, fty) {
                spec_fields.push((fname, new_fty));
                spec_tag.push_str(&format!("_{idx}"));
                real_change = true;
            } else if aggregate(ctx, fty) {
                let mut field_objects = BTreeSet::new();
                for &s in &field_slots {
                    field_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if let Some((new_fty, sub_real)) =
                    self.specialized_type_inner(ctx, fty, &field_objects, visiting)
                {
                    let tag = type_spec_tag(ctx, new_fty);
                    spec_fields.push((fname, new_fty));
                    spec_tag.push_str(&format!("_{idx}_{tag}"));
                    if sub_real {
                        real_change = true;
                    }
                } else {
                    spec_fields.push((fname, fty));
                }
            } else {
                spec_fields.push((fname, fty));
            }
        }
        if !real_change {
            if is_rec {
                visiting.remove(&ty);
            }
            return None;
        }
        let handle = if let Some(reserved_name) = reserved_name {
            if is_tuple {
                RecordType::get_tuple(ctx, reserved_name, Some(spec_fields)).ok()?
            } else {
                RecordType::get(ctx, reserved_name, Some(spec_fields)).ok()?
            }
        } else {
            // The suffix only appends identifier characters, so a fresh legaliser keeps the name.
            let new_name = Legaliser::default().legalise(&format!("{rec_name}__num{spec_tag}"));
            if is_tuple {
                RecordType::get_tuple(ctx, new_name, Some(spec_fields)).ok()?
            } else {
                RecordType::get(ctx, new_name, Some(spec_fields)).ok()?
            }
        };
        Some((handle.into(), true))
    }
}

/// The numeric representation of an `f64` (or optional `f64`) slot type.
fn numeric_type_for(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    if ty.deref(ctx).is::<pliron::builtin::types::FP64Type>() {
        return Some(crate::types::NumericType::get(ctx).into());
    }
    if let Some(opt) = ty.deref(ctx).downcast_ref::<crate::types::OptionalType>()
        && opt
            .value()
            .deref(ctx)
            .is::<pliron::builtin::types::FP64Type>()
    {
        return Some(
            crate::types::OptionalType::get_with(
                ctx,
                crate::types::NumericType::get(ctx).into(),
                opt.absence(),
            )
            .into(),
        );
    }
    None
}

/// An identifier-safe tag distinguishing nested specialized layouts in record names.
fn type_spec_tag(ctx: &Context, ty: TypeHandle) -> String {
    if let Some(opt) = ty.deref(ctx).downcast_ref::<crate::types::OptionalType>() {
        return format!("opt_{}", type_spec_tag(ctx, opt.value()));
    }
    if let Some(arr) = ty.deref(ctx).downcast_ref::<ArrayType>() {
        return format!("arr_{}", type_spec_tag(ctx, arr.element()));
    }
    if let Some(rec) = ty.deref(ctx).downcast_ref::<RecordType>() {
        let s: String = rec
            .name()
            .as_ref()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        return format!("rec_{s}");
    }
    if ty.deref(ctx).is::<crate::types::NumericType>() {
        return "num".to_string();
    }
    "scalar".to_string()
}

/// Whether `ty` reaches a record already on the `visiting` path.
fn is_recursive_type(ctx: &Context, ty: TypeHandle, visiting: &mut HashSet<TypeHandle>) -> bool {
    let shape = ty.deref(ctx);
    if let Some(opt) = shape.downcast_ref::<crate::types::OptionalType>() {
        return is_recursive_type(ctx, opt.value(), visiting);
    }
    if let Some(arr) = shape.downcast_ref::<ArrayType>() {
        return is_recursive_type(ctx, arr.element(), visiting);
    }
    let Some(record) = shape.downcast_ref::<RecordType>() else {
        return false;
    };
    if !visiting.insert(ty) {
        return true;
    }
    // An opaque record has no fields to specialize; `specialized_objects_type` rejects it.
    let recursive = record.fields().is_ok_and(|fields| {
        fields
            .iter()
            .any(|&(_, field)| is_recursive_type(ctx, field, visiting))
    });
    visiting.remove(&ty);
    recursive
}
