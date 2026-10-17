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
        if aggregate(ctx, ty)
            && !ty.deref(ctx).is::<PromiseType>()
            && let Some(&cell) = self.states[id].heap_values.get(&value)
            && let Some(spec) = self.specialized_cell_type(ctx, ty, cell)
        {
            return Some(spec);
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
            if Operation::get_op::<UnwrapOp>(op, ctx).is_some()
                || Operation::get_op::<crate::optional::ToNumberOp>(op, ctx).is_some()
            {
                let base = op.deref(ctx).get_operand(0);
                if let Some(base_spec) = self.specialized_value_type(ctx, id, base) {
                    let inner_ty = crate::optional::optional_value(ctx, base_spec)?;
                    return (inner_ty != ty).then_some(inner_ty);
                }
            }
            if Operation::get_op::<WrapOp>(op, ctx).is_some()
                || Operation::get_op::<OptionalCastOp>(op, ctx).is_some()
            {
                let base = op.deref(ctx).get_operand(0);
                if let Some(base_spec) = self.specialized_value_type(ctx, id, base) {
                    let payload_spec = if Operation::get_op::<OptionalCastOp>(op, ctx).is_some() {
                        crate::optional::optional_value(ctx, base_spec)?
                    } else {
                        base_spec
                    };
                    let absence = ty
                        .deref(ctx)
                        .downcast_ref::<crate::types::OptionalType>()?
                        .absence();
                    let opt_ty: TypeHandle =
                        crate::types::OptionalType::get_with(ctx, payload_spec, absence).into();
                    return (opt_ty != ty).then_some(opt_ty);
                }
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
        if state.fact(value) == Fact::Numeric
            || (state.fact(value).needs_lossless_64()
                && !super::super::rewrite::can_specialize_value(ctx, state, value))
        {
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
        if res.domain == Fact::Numeric
            || (res.domain.needs_lossless_64() && ty.deref(ctx).is::<crate::types::OptionalType>())
        {
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
            return Some((target, true));
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
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
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
        let (is_tuple, fields) = {
            let deref = ty.deref(ctx);
            let rec = deref.downcast_ref::<RecordType>()?;
            (rec.is_tuple(), rec.fields().ok()?.to_vec())
        };
        let (layout_key, _) = self.layout_key(ctx, ty, objects, &mut Vec::new())?;
        let record_name = layout_key.record_identifier();
        let is_rec = is_recursive_type(ctx, ty, &mut HashSet::new());
        if is_rec {
            let reserved_handle: TypeHandle = if is_tuple {
                RecordType::get_tuple(ctx, record_name.clone(), None)
                    .expect("recursive tuple layout reservation must succeed")
                    .into()
            } else {
                RecordType::get(ctx, record_name.clone(), None)
                    .expect("recursive record layout reservation must succeed")
                    .into()
            };
            visiting.insert(ty, reserved_handle);
        }
        let mut real_change = false;
        let mut spec_fields = Vec::with_capacity(fields.len());
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
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
            if all_numeric && let Some(new_fty) = numeric_type_for(ctx, fty) {
                spec_fields.push((fname, new_fty));
                real_change = true;
            } else if aggregate(ctx, fty) {
                let mut field_objects = BTreeSet::new();
                for &s in &field_slots {
                    field_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if let Some((new_fty, sub_real)) =
                    self.specialized_type_inner(ctx, fty, &field_objects, visiting)
                {
                    spec_fields.push((fname, new_fty));
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
        let handle = if is_tuple {
            RecordType::get_tuple(ctx, record_name, Some(spec_fields))
                .expect("specialized tuple layout must be consistent")
        } else {
            RecordType::get(ctx, record_name, Some(spec_fields))
                .expect("specialized record layout must be consistent")
        };
        if is_rec {
            visiting.remove(&ty);
        }
        Some((handle.into(), true))
    }

    fn has_numeric_specialization(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut HashSet<TypeHandle>,
    ) -> bool {
        if objects.is_empty() {
            return false;
        }
        if let Some(opt) = ty.deref(ctx).downcast_ref::<crate::types::OptionalType>() {
            return self.has_numeric_specialization(ctx, opt.value(), objects, visiting);
        }
        if let Some(promise) = ty.deref(ctx).downcast_ref::<crate::types::PromiseType>() {
            return self.has_numeric_specialization(ctx, promise.value(), objects, visiting);
        }
        if !visiting.insert(ty) {
            return false;
        }
        let array_elem = ty
            .deref(ctx)
            .downcast_ref::<ArrayType>()
            .map(|a| a.element());
        if let Some(elem_ty) = array_elem {
            let mut element_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Array { element, .. } = &self.heap.objects[o].contents else {
                    visiting.remove(&ty);
                    return false;
                };
                element_slots.push(*element);
            }
            let all_numeric = element_slots
                .iter()
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
            if all_numeric && numeric_type_for(ctx, elem_ty).is_some() {
                visiting.remove(&ty);
                return true;
            }
            if aggregate(ctx, elem_ty) {
                let mut elem_objects = BTreeSet::new();
                for &s in &element_slots {
                    elem_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                let res = self.has_numeric_specialization(ctx, elem_ty, &elem_objects, visiting);
                visiting.remove(&ty);
                return res;
            }
            visiting.remove(&ty);
            return false;
        }
        let fields = {
            let deref = ty.deref(ctx);
            let Some(rec) = deref.downcast_ref::<RecordType>() else {
                visiting.remove(&ty);
                return false;
            };
            let Ok(fields) = rec.fields() else {
                visiting.remove(&ty);
                return false;
            };
            fields.to_vec()
        };
        for (idx, (_fname, fty)) in fields.into_iter().enumerate() {
            let mut field_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Record(slots) = &self.heap.objects[o].contents else {
                    visiting.remove(&ty);
                    return false;
                };
                let Some(&s) = slots.get(idx) else {
                    visiting.remove(&ty);
                    return false;
                };
                field_slots.push(s);
            }
            let all_numeric = field_slots
                .iter()
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
            if all_numeric && numeric_type_for(ctx, fty).is_some() {
                visiting.remove(&ty);
                return true;
            }
            if aggregate(ctx, fty) {
                let mut field_objects = BTreeSet::new();
                for &s in &field_slots {
                    field_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if self.has_numeric_specialization(ctx, fty, &field_objects, visiting) {
                    visiting.remove(&ty);
                    return true;
                }
            }
        }
        visiting.remove(&ty);
        false
    }

    fn layout_key(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut Vec<TypeHandle>,
    ) -> Option<(LayoutKey, bool)> {
        if objects.is_empty() {
            return None;
        }
        if let Some(pos) = visiting.iter().position(|&v_ty| v_ty == ty) {
            let depth = visiting.len() - 1 - pos;
            let real = self.has_numeric_specialization(ctx, ty, objects, &mut HashSet::new());
            return Some((LayoutKey::SelfRef(depth), real));
        }
        if let Some(opt) = ty.deref(ctx).downcast_ref::<crate::types::OptionalType>() {
            let (inner, absence) = (opt.value(), opt.absence());
            let (inner_key, real) = self.layout_key(ctx, inner, objects, visiting)?;
            return Some((
                LayoutKey::Optional {
                    absence: absence as u8,
                    inner: Box::new(inner_key),
                },
                real,
            ));
        }
        if let Some(promise) = ty.deref(ctx).downcast_ref::<crate::types::PromiseType>() {
            let inner = promise.value();
            let (inner_key, real) = self.layout_key(ctx, inner, objects, visiting)?;
            return Some((LayoutKey::Promise(Box::new(inner_key)), real));
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
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
            if all_numeric && numeric_type_for(ctx, elem_ty).is_some() {
                return Some((LayoutKey::Array(Box::new(LayoutKey::Numeric)), true));
            }
            if aggregate(ctx, elem_ty) {
                let mut elem_objects = BTreeSet::new();
                for &s in &element_slots {
                    elem_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if let Some((sub_key, real)) =
                    self.layout_key(ctx, elem_ty, &elem_objects, visiting)
                {
                    return Some((LayoutKey::Array(Box::new(sub_key)), real));
                }
            }
            return None;
        }
        let (rec_name, is_tuple, fields) = {
            let deref = ty.deref(ctx);
            let rec = deref.downcast_ref::<RecordType>()?;
            (
                rec.name().clone(),
                rec.is_tuple(),
                rec.fields().ok()?.to_vec(),
            )
        };
        visiting.push(ty);
        let mut real_change = false;
        let mut specialized_fields = Vec::new();
        for (idx, (_fname, fty)) in fields.into_iter().enumerate() {
            let mut field_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Record(slots) = &self.heap.objects[o].contents else {
                    visiting.pop();
                    return None;
                };
                let Some(&s) = slots.get(idx) else {
                    visiting.pop();
                    return None;
                };
                field_slots.push(s);
            }
            let all_numeric = field_slots
                .iter()
                .all(|&s| self.heap.cells[s].domain.needs_lossless_64());
            if all_numeric && numeric_type_for(ctx, fty).is_some() {
                specialized_fields.push((idx, LayoutKey::Numeric));
                real_change = true;
            } else if aggregate(ctx, fty) {
                let mut field_objects = BTreeSet::new();
                for &s in &field_slots {
                    field_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                if let Some((sub_key, sub_real)) =
                    self.layout_key(ctx, fty, &field_objects, visiting)
                {
                    specialized_fields.push((idx, sub_key));
                    if sub_real {
                        real_change = true;
                    }
                }
            }
        }
        visiting.pop();
        real_change.then_some((
            LayoutKey::Record {
                base_name: rec_name,
                is_tuple,
                specialized_fields,
            },
            true,
        ))
    }

    pub(in crate::facts) fn verify_cell_layout_lossless(
        &self,
        ctx: &Context,
        loc: pliron::location::Location,
        ty: TypeHandle,
        cell: usize,
    ) -> pliron::result::Result<()> {
        self.verify_objects_layout_lossless(
            ctx,
            loc,
            ty,
            &self.heap.cells[cell].layout_objects,
            &mut HashSet::new(),
        )
    }

    fn verify_objects_layout_lossless(
        &self,
        ctx: &Context,
        loc: pliron::location::Location,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut HashSet<TypeHandle>,
    ) -> pliron::result::Result<()> {
        use crate::facts::rewrite::LosslessRepresentationErr;
        use pliron::verify_err;
        let ty = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
        if objects.is_empty() || !visiting.insert(ty) {
            return Ok(());
        }
        let array_elem = ty
            .deref(ctx)
            .downcast_ref::<ArrayType>()
            .map(|a| a.element());
        if let Some(elem_ty) = array_elem {
            let mut element_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Array { element, .. } = &self.heap.objects[o].contents else {
                    visiting.remove(&ty);
                    return Ok(());
                };
                element_slots.push(*element);
            }
            let requires_lossless = element_slots.iter().any(|&s| {
                self.heap.cells[s].domain == Fact::Numeric
                    || self.heap.cells[s]
                        .domain
                        .integer()
                        .is_some_and(|d| d.bits == 64)
            });
            if requires_lossless && numeric_type_for(ctx, elem_ty).is_some() {
                visiting.remove(&ty);
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(
                        "array element with Fact::Numeric has f64 carrier".into()
                    )
                );
            }
            if aggregate(ctx, elem_ty) {
                let mut elem_objects = BTreeSet::new();
                for &s in &element_slots {
                    elem_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                self.verify_objects_layout_lossless(ctx, loc, elem_ty, &elem_objects, visiting)?;
            }
            visiting.remove(&ty);
            return Ok(());
        }
        let fields = {
            let deref = ty.deref(ctx);
            let Some(rec) = deref.downcast_ref::<RecordType>() else {
                visiting.remove(&ty);
                return Ok(());
            };
            let Ok(fields) = rec.fields() else {
                visiting.remove(&ty);
                return Ok(());
            };
            fields.to_vec()
        };
        for (idx, (fname, fty)) in fields.into_iter().enumerate() {
            let mut field_slots = Vec::with_capacity(objects.len());
            for &o in objects {
                let Contents::Record(slots) = &self.heap.objects[o].contents else {
                    visiting.remove(&ty);
                    return Ok(());
                };
                let Some(&s) = slots.get(idx) else {
                    visiting.remove(&ty);
                    return Ok(());
                };
                field_slots.push(s);
            }
            let requires_lossless = field_slots.iter().any(|&s| {
                self.heap.cells[s].domain == Fact::Numeric
                    || self.heap.cells[s]
                        .domain
                        .integer()
                        .is_some_and(|d| d.bits == 64)
            });
            if requires_lossless && numeric_type_for(ctx, fty).is_some() {
                visiting.remove(&ty);
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(format!(
                        "record field `{fname}` with Fact::Numeric has f64 carrier"
                    ))
                );
            }
            if aggregate(ctx, fty) {
                let mut field_objects = BTreeSet::new();
                for &s in &field_slots {
                    field_objects.extend(self.heap.cells[s].layout_objects.iter().copied());
                }
                self.verify_objects_layout_lossless(
                    ctx,
                    loc.clone(),
                    fty,
                    &field_objects,
                    visiting,
                )?;
            }
        }
        visiting.remove(&ty);
        Ok(())
    }
}

/// Canonical structural key for a specialized aggregate layout.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum LayoutKey {
    SelfRef(usize),
    Numeric,
    Optional {
        absence: u8,
        inner: Box<Self>,
    },
    Promise(Box<Self>),
    Array(Box<Self>),
    Record {
        base_name: Identifier,
        is_tuple: bool,
        specialized_fields: Vec<(usize, Self)>,
    },
}

impl LayoutKey {
    fn record_identifier(&self) -> Identifier {
        let mut out = String::from("perry_layout_");
        self.write_encoded(&mut out);
        Legaliser::default().legalise(&out)
    }

    fn write_encoded(&self, out: &mut String) {
        use std::fmt::Write;
        match self {
            Self::SelfRef(depth) => {
                let _ = write!(out, "S{depth}_");
            }
            Self::Numeric => out.push('N'),
            Self::Optional { absence, inner } => {
                let _ = write!(out, "O{absence}_");
                inner.write_encoded(out);
            }
            Self::Promise(inner) => {
                out.push('P');
                inner.write_encoded(out);
            }
            Self::Array(inner) => {
                out.push('A');
                inner.write_encoded(out);
            }
            Self::Record {
                base_name,
                is_tuple,
                specialized_fields,
            } => {
                out.push(if *is_tuple { 'T' } else { 'R' });
                write_ident(out, base_name.as_ref());
                let _ = write!(out, "{}_", specialized_fields.len());
                for (idx, field_key) in specialized_fields {
                    let _ = write!(out, "F{idx}_");
                    field_key.write_encoded(out);
                }
            }
        }
    }
}

fn write_ident(out: &mut String, ident: &str) {
    use std::fmt::Write;
    let mut encoded = String::with_capacity(ident.len());
    for ch in ident.chars() {
        if (ch.is_ascii_alphanumeric() && ch != 'X') || ch == '_' {
            encoded.push(ch);
        } else {
            let _ = write!(encoded, "X{:x}_", ch as u32);
        }
    }
    let _ = write!(out, "{}_{encoded}_", encoded.len());
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
