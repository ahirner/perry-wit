//! Numeric layout specialization of aggregate types from converged slot domains.
use super::*;
use pliron::identifier::Legaliser;
use std::cell::Ref;

impl ProgramFacts {
    /// The specialized type of `value` in context `id`, if its layout or domain changes.
    pub(in crate::facts) fn specialized_value_type(
        &self,
        ctx: &Context,
        id: usize,
        value: Value,
    ) -> Result<Option<TypeHandle>> {
        let ty = value.get_type(ctx);
        if aggregate(ctx, ty)
            && !ty.deref(ctx).is::<PromiseType>()
            && let Some(&cell) = self.states[id].heap_values.get(&value)
            && let Some(spec) = self.specialized_cell_type(ctx, ty, cell)?
        {
            return Ok(Some(spec));
        }
        let state = &self.states[id];
        if let Some(op) = value.defining_op() {
            // A projection of a specialized base takes the projected specialized type.
            let changed = |projected: TypeHandle| (projected != ty).then_some(projected);
            if crate::is_any::<(
                FieldGetOp,
                ArrayAtOp,
                ArrayGetOp,
                UnwrapOp,
                crate::optional::ToNumberOp,
                WrapOp,
                OptionalCastOp,
            )>(ctx, op)
            {
                let base = self.specialized_value_type(ctx, id, op.deref(ctx).get_operand(0))?;
                if let Some(base) = base {
                    return Ok(projected_type(ctx, op, base, ty).and_then(changed));
                }
                if crate::is_any::<(FieldGetOp, ArrayAtOp, ArrayGetOp)>(ctx, op) {
                    return Ok(None);
                }
            }
            if crate::is_any::<(crate::locals::LoadOp, AwaitOp)>(ctx, op) {
                return Ok(None);
            }
            if let Some(intrinsic) = crate::intrinsics::intrinsic_of(ctx, op) {
                let integral = super::super::rewrite::integral_intrinsic(ctx, op, intrinsic, |v| {
                    state.representation(v).is_some()
                });
                let numeric = intrinsic.preserves_integers()
                    && op
                        .deref(ctx)
                        .operands()
                        .any(|v| state.fact(v) == Fact::Numeric);
                if !integral && !numeric {
                    return Ok(None);
                }
            }
        }
        if state.fact(value) == Fact::Numeric
            || (state.fact(value).needs_lossless_64()
                && !super::super::rewrite::can_specialize_value(ctx, state, value))
        {
            return Ok(numeric_type_for(ctx, ty));
        }
        Ok(None)
    }

    /// The specialized type of result `pos` of context `id`, declared as `ty`.
    pub(in crate::facts) fn specialized_result_type(
        &self,
        ctx: &Context,
        id: usize,
        pos: usize,
        ty: TypeHandle,
    ) -> Result<Option<TypeHandle>> {
        let res = &self.states[id].results[pos];
        if aggregate(ctx, ty) && !ty.deref(ctx).is::<PromiseType>() {
            return res
                .heap
                .map(|cell| self.specialized_cell_type(ctx, ty, cell))
                .transpose()
                .map(Option::flatten);
        }
        if res.domain == Fact::Numeric
            || (res.domain.needs_lossless_64() && ty.deref(ctx).is::<crate::types::OptionalType>())
        {
            return Ok(numeric_type_for(ctx, ty));
        }
        Ok(None)
    }

    fn specialized_cell_type(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        cell: usize,
    ) -> Result<Option<TypeHandle>> {
        let objects = &self.heap.cells[cell].layout_objects;
        self.specialized_type_inner(ctx, ty, objects, &mut HashMap::new())
    }

    /// Specializes `ty` when every object in `objects` agrees on numeric slots.
    fn specialized_type_inner(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut HashMap<TypeHandle, TypeHandle>,
    ) -> Result<Option<TypeHandle>> {
        if let Some(&target) = visiting.get(&ty) {
            return Ok(Some(target));
        }
        if let Some((inner, absence)) = crate::optional::optional_parts(ctx, ty) {
            return Ok(self
                .specialized_type_inner(ctx, inner, objects, visiting)?
                .map(|inner| crate::types::OptionalType::get_with(ctx, inner, absence).into()));
        }
        if objects.is_empty() {
            return Ok(None);
        }
        let Some(layout) = Layout::of(ctx, ty) else {
            return Ok(None);
        };
        match layout {
            Layout::Array(element) => {
                let specialized = match self.heap.step(ctx, objects, None, element, Lossless::All) {
                    Some(Step::Numeric(element)) => Some(element),
                    Some(Step::Nested(nested)) => {
                        self.specialized_type_inner(ctx, element, &nested, visiting)?
                    }
                    Some(Step::Keep) | None => None,
                };
                Ok(specialized.map(|element| ArrayType::get(ctx, element).into()))
            }
            Layout::Record { kind, fields, .. } => {
                let Some((key, _)) = self.layout_key(ctx, ty, objects, &mut Vec::new()) else {
                    return Ok(None);
                };
                let name = key.record_identifier();
                let get = |ctx: &Context, fields: Option<Vec<(String, TypeHandle)>>| {
                    RecordType::get_kind(ctx, name.clone(), fields, kind)
                };
                // Reserving before the fields also handles recursive records without a pre-walk.
                visiting.insert(ty, get(ctx, None)?.into());
                let fields = self.specialized_fields(ctx, fields.to_vec(), objects, visiting);
                visiting.remove(&ty);
                fields?
                    .map(|fields| get(ctx, Some(fields)).map(Into::into))
                    .transpose()
            }
        }
    }

    /// The fields of a record layout that `objects` share, with at least one specialized.
    fn specialized_fields(
        &self,
        ctx: &Context,
        mut fields: Vec<(String, TypeHandle)>,
        objects: &BTreeSet<usize>,
        visiting: &mut HashMap<TypeHandle, TypeHandle>,
    ) -> Result<Option<Vec<(String, TypeHandle)>>> {
        let mut real_change = false;
        for (index, (_, ty)) in fields.iter_mut().enumerate() {
            let Some(step) = self
                .heap
                .step(ctx, objects, Some(index), *ty, Lossless::All)
            else {
                return Ok(None);
            };
            let specialized = match step {
                Step::Numeric(numeric) => Some(numeric),
                Step::Nested(nested) => self.specialized_type_inner(ctx, *ty, &nested, visiting)?,
                Step::Keep => None,
            };
            if let Some(specialized) = specialized {
                real_change = true;
                *ty = specialized;
            }
        }
        Ok(real_change.then_some(fields))
    }

    /// Whether some slot group `objects` share under `ty` specializes to a numeric carrier.
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
        if let Some(inner) = payload_step(ctx, ty) {
            return self.has_numeric_specialization(ctx, inner, objects, visiting);
        }
        if !visiting.insert(ty) {
            return false;
        }
        let found = Layout::of(ctx, ty).is_some_and(|layout| {
            for (field, _, child) in layout.children() {
                let numeric = match self.heap.step(ctx, objects, field, child, Lossless::All) {
                    None => return false,
                    Some(Step::Numeric(_)) => true,
                    Some(Step::Nested(nested)) => {
                        self.has_numeric_specialization(ctx, child, &nested, visiting)
                    }
                    Some(Step::Keep) => false,
                };
                if numeric {
                    return true;
                }
            }
            false
        });
        visiting.remove(&ty);
        found
    }

    /// The structural key of the layout `objects` share under `ty`, relative to the records on
    /// the `visiting` path, and whether it specializes anything.
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
        if let Some((inner, absence)) = crate::optional::optional_parts(ctx, ty) {
            let (inner, real) = self.layout_key(ctx, inner, objects, visiting)?;
            let absence = absence as u8;
            let inner = Box::new(inner);
            return Some((LayoutKey::Optional { absence, inner }, real));
        }
        if let Some(inner) = payload_step(ctx, ty) {
            let (inner, real) = self.layout_key(ctx, inner, objects, visiting)?;
            return Some((LayoutKey::Promise(Box::new(inner)), real));
        }
        match Layout::of(ctx, ty)? {
            Layout::Array(element) => {
                let (key, real) =
                    match self.heap.step(ctx, objects, None, element, Lossless::All)? {
                        Step::Numeric(_) => (LayoutKey::Numeric, true),
                        Step::Nested(nested) => self.layout_key(ctx, element, &nested, visiting)?,
                        Step::Keep => return None,
                    };
                Some((LayoutKey::Array(Box::new(key)), real))
            }
            Layout::Record {
                name: base_name,
                kind,
                fields,
            } => {
                visiting.push(ty);
                let fields = self.layout_key_fields(ctx, &fields, objects, visiting);
                visiting.pop();
                let specialized_fields = fields?;
                let key = LayoutKey::Record {
                    base_name: base_name.clone(),
                    is_tuple: kind != crate::RecordKind::Object,
                    specialized_fields,
                };
                Some((key, true))
            }
        }
    }

    /// The keys of the specialized fields of a record layout, if at least one changes.
    fn layout_key_fields(
        &self,
        ctx: &Context,
        fields: &[(String, TypeHandle)],
        objects: &BTreeSet<usize>,
        visiting: &mut Vec<TypeHandle>,
    ) -> Option<Vec<(usize, LayoutKey)>> {
        let mut real_change = false;
        let mut specialized = Vec::new();
        for (index, &(_, ty)) in fields.iter().enumerate() {
            match self
                .heap
                .step(ctx, objects, Some(index), ty, Lossless::All)?
            {
                Step::Numeric(_) => {
                    specialized.push((index, LayoutKey::Numeric));
                    real_change = true;
                }
                Step::Nested(nested) => {
                    if let Some((key, real)) = self.layout_key(ctx, ty, &nested, visiting) {
                        specialized.push((index, key));
                        real_change |= real;
                    }
                }
                Step::Keep => {}
            }
        }
        real_change.then_some(specialized)
    }

    pub(in crate::facts) fn verify_cell_layout_lossless(
        &self,
        ctx: &Context,
        loc: &pliron::location::Location,
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

    /// Fails when a slot group that needs a lossless carrier is still typed `f64` in `ty`.
    fn verify_objects_layout_lossless(
        &self,
        ctx: &Context,
        loc: &pliron::location::Location,
        ty: TypeHandle,
        objects: &BTreeSet<usize>,
        visiting: &mut HashSet<TypeHandle>,
    ) -> pliron::result::Result<()> {
        let ty = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
        if objects.is_empty() || !visiting.insert(ty) {
            return Ok(());
        }
        let verified = Layout::of(ctx, ty).map_or(Ok(()), |layout| {
            for (field, name, child) in layout.children() {
                // A shape the objects do not share specializes nothing.
                let Some(step) = self.heap.step(ctx, objects, field, child, Lossless::Any) else {
                    return Ok(());
                };
                if let Step::Numeric(_) = step {
                    let message = name.map_or_else(
                        || "array element with Fact::Numeric has f64 carrier".to_string(),
                        |name| format!("record field `{name}` with Fact::Numeric has f64 carrier"),
                    );
                    return pliron::verify_err!(
                        loc.clone(),
                        crate::facts::rewrite::LosslessRepresentationErr(message)
                    );
                }
                if let Step::Nested(nested) = step {
                    self.verify_objects_layout_lossless(ctx, loc, child, &nested, visiting)?;
                }
            }
            Ok(())
        });
        visiting.remove(&ty);
        verified
    }
}

/// Projects a specialized base through the access or optional conversion that produced `ty`.
fn projected_type(
    ctx: &Context,
    op: Ptr<Operation>,
    base: TypeHandle,
    ty: TypeHandle,
) -> Option<TypeHandle> {
    if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
        return base
            .deref(ctx)
            .downcast_ref::<RecordType>()?
            .field(&get.field(ctx))
            .map(|(_, ty)| ty);
    }
    if crate::is_any::<(ArrayAtOp, ArrayGetOp)>(ctx, op) {
        let element = base.deref(ctx).downcast_ref::<ArrayType>()?.element();
        return Some(if Operation::is_op::<ArrayGetOp>(op, ctx) {
            crate::types::OptionalType::get(ctx, element).into()
        } else {
            element
        });
    }
    if crate::is_any::<(UnwrapOp, crate::optional::ToNumberOp)>(ctx, op) {
        return crate::optional::optional_value(ctx, base);
    }
    let payload = if Operation::is_op::<OptionalCastOp>(op, ctx) {
        crate::optional::optional_value(ctx, base)?
    } else {
        base
    };
    let absence = crate::optional::absence(ctx, ty)?;
    Some(crate::types::OptionalType::get_with(ctx, payload, absence).into())
}

/// Borrowed children avoid copying fields during read-only layout walks.
enum Layout<'a> {
    Array(TypeHandle),
    Record {
        name: Ref<'a, Identifier>,
        kind: crate::RecordKind,
        fields: Ref<'a, [(String, TypeHandle)]>,
    },
}

impl<'a> Layout<'a> {
    /// `None` for other types and for opaque records, which have nothing to specialize.
    fn of(ctx: &'a Context, ty: TypeHandle) -> Option<Self> {
        let ty = ty.deref(ctx);
        if let Some(array) = ty.downcast_ref::<ArrayType>() {
            return Some(Self::Array(array.element()));
        }
        let record = Ref::filter_map(ty, |ty| ty.downcast_ref::<RecordType>()).ok()?;
        Some(Self::Record {
            name: Ref::map(Ref::clone(&record), RecordType::name),
            kind: record.kind(),
            fields: Ref::filter_map(record, |record| record.fields().ok()).ok()?,
        })
    }

    /// `(field index, field name, type)` of each child; an array's one child is its element.
    fn children(&self) -> impl Iterator<Item = (Option<usize>, Option<&str>, TypeHandle)> {
        let (element, fields) = match self {
            Self::Array(element) => (Some((None, None, *element)), &[][..]),
            Self::Record { fields, .. } => (None, &fields[..]),
        };
        element.into_iter().chain(
            fields
                .iter()
                .enumerate()
                .map(|(index, (name, ty))| (Some(index), Some(name.as_str()), *ty)),
        )
    }
}

/// How the slots holding one child of every object in a group specialize.
enum Step {
    /// The lossless requirement holds for an `f64` child: its numeric carrier.
    Numeric(TypeHandle),
    /// The child is an aggregate, decided by the objects its slots may hold.
    Nested(BTreeSet<usize>),
    Keep,
}

/// Specialization needs agreement; verification rejects even one lossy slot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lossless {
    All,
    Any,
}

impl HeapFacts {
    /// Classify a child without materializing its slot list; reject mismatched object shapes.
    fn step(
        &self,
        ctx: &Context,
        objects: &BTreeSet<usize>,
        field: Option<usize>,
        ty: TypeHandle,
        requirement: Lossless,
    ) -> Option<Step> {
        let mut lossless = requirement == Lossless::All;
        let is_aggregate = aggregate(ctx, ty);
        let mut nested = BTreeSet::new();
        for &object in objects {
            let slot = match (&self.objects[object].contents, field) {
                (Contents::Array { element, .. }, None) => *element,
                (Contents::Record(slots), Some(index)) => *slots.get(index)?,
                _ => return None,
            };
            let cell = &self.cells[slot];
            let needs_lossless = cell.domain.needs_lossless_64();
            match requirement {
                Lossless::All => lossless &= needs_lossless,
                Lossless::Any => lossless |= needs_lossless,
            }
            if is_aggregate {
                nested.extend(cell.layout_objects.iter().copied());
            }
        }
        Some(match numeric_type_for(ctx, ty) {
            Some(numeric) if lossless => Step::Numeric(numeric),
            _ if is_aggregate => Step::Nested(nested),
            _ => Step::Keep,
        })
    }
}

/// The payload of an optional or promise type, which share their payload's layout.
fn payload_step(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    let ty = ty.deref(ctx);
    ty.downcast_ref::<crate::types::OptionalType>()
        .map(|optional| optional.value())
        .or_else(|| {
            ty.downcast_ref::<PromiseType>()
                .map(|promise| promise.value())
        })
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
    let is_number = |ty: TypeHandle| ty.deref(ctx).is::<pliron::builtin::types::FP64Type>();
    let numeric = crate::types::NumericType::get(ctx).into();
    if is_number(ty) {
        return Some(numeric);
    }
    let (value, absence) = crate::optional::optional_parts(ctx, ty)?;
    is_number(value).then(|| crate::types::OptionalType::get_with(ctx, numeric, absence).into())
}
