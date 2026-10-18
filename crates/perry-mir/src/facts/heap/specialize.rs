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
            // A projection of a specialized base takes the projected specialized type.
            let base = || self.specialized_value_type(ctx, id, op.deref(ctx).get_operand(0));
            let changed = |projected: TypeHandle| (projected != ty).then_some(projected);
            if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
                let record = base()?;
                let field = record
                    .deref(ctx)
                    .downcast_ref::<RecordType>()?
                    .field(&get.field(ctx));
                return changed(field?.1);
            }
            if crate::is_any::<(ArrayAtOp, ArrayGetOp)>(ctx, op) {
                let element = base()?.deref(ctx).downcast_ref::<ArrayType>()?.element();
                return changed(if Operation::is_op::<ArrayGetOp>(op, ctx) {
                    crate::types::OptionalType::get(ctx, element).into()
                } else {
                    element
                });
            }
            if crate::is_any::<(UnwrapOp, crate::optional::ToNumberOp)>(ctx, op)
                && let Some(optional) = base()
            {
                return changed(crate::optional::optional_value(ctx, optional)?);
            }
            if crate::is_any::<(WrapOp, OptionalCastOp)>(ctx, op)
                && let Some(payload) = base()
            {
                let payload = if Operation::is_op::<OptionalCastOp>(op, ctx) {
                    crate::optional::optional_value(ctx, payload)?
                } else {
                    payload
                };
                let absence = crate::optional::absence(ctx, ty)?;
                return changed(crate::types::OptionalType::get_with(ctx, payload, absence).into());
            }
            if crate::is_any::<(crate::locals::LoadOp, AwaitOp)>(ctx, op) {
                return None;
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
        let objects = &self.heap.cells[cell].layout_objects;
        let (t, real) = self.specialized_type_inner(ctx, ty, objects, &mut HashMap::new())?;
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
        if let Some((inner, absence)) = crate::optional::optional_parts(ctx, ty) {
            let (inner, real) = self.specialized_type_inner(ctx, inner, objects, visiting)?;
            return Some((
                crate::types::OptionalType::get_with(ctx, inner, absence).into(),
                real,
            ));
        }
        if objects.is_empty() {
            return None;
        }
        match Layout::of(ctx, ty)? {
            Layout::Array(element) => {
                let (element, real) = match self.heap.step(ctx, objects, None, element)? {
                    Step::Numeric(element) => (element, true),
                    Step::Nested(nested) => {
                        self.specialized_type_inner(ctx, element, &nested, visiting)?
                    }
                    Step::Keep => return None,
                };
                Some((ArrayType::get(ctx, element).into(), real))
            }
            Layout::Record { tuple, fields, .. } => {
                let (key, _) = self.layout_key(ctx, ty, objects, &mut Vec::new())?;
                let name = key.record_identifier();
                let get = |ctx: &Context, fields: Option<Vec<(String, TypeHandle)>>| {
                    let record = if tuple {
                        RecordType::get_tuple(ctx, name.clone(), fields)
                    } else {
                        RecordType::get(ctx, name.clone(), fields)
                    };
                    record.expect("a specialized layout is consistent with its reservation")
                };
                // A recursive record reserves its name, so its own fields can refer to it.
                let recursive = is_recursive_type(ctx, ty, &mut HashSet::new());
                if recursive {
                    visiting.insert(ty, get(ctx, None).into());
                }
                let fields = self.specialized_fields(ctx, fields, objects, visiting);
                if recursive {
                    visiting.remove(&ty);
                }
                Some((get(ctx, Some(fields?)).into(), true))
            }
        }
    }

    /// The fields of a record layout that `objects` share, with at least one specialized.
    fn specialized_fields(
        &self,
        ctx: &Context,
        fields: Vec<(String, TypeHandle)>,
        objects: &BTreeSet<usize>,
        visiting: &mut HashMap<TypeHandle, TypeHandle>,
    ) -> Option<Vec<(String, TypeHandle)>> {
        let mut real_change = false;
        let mut specialized = Vec::with_capacity(fields.len());
        for (index, (name, ty)) in fields.into_iter().enumerate() {
            let ty = match self.heap.step(ctx, objects, Some(index), ty)? {
                Step::Numeric(numeric) => {
                    real_change = true;
                    numeric
                }
                Step::Nested(nested) => {
                    match self.specialized_type_inner(ctx, ty, &nested, visiting) {
                        Some((nested, real)) => {
                            real_change |= real;
                            nested
                        }
                        None => ty,
                    }
                }
                Step::Keep => ty,
            };
            specialized.push((name, ty));
        }
        real_change.then_some(specialized)
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
                let numeric = match self.heap.step(ctx, objects, field, child) {
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
                let (key, real) = match self.heap.step(ctx, objects, None, element)? {
                    Step::Numeric(_) => (LayoutKey::Numeric, true),
                    Step::Nested(nested) => self.layout_key(ctx, element, &nested, visiting)?,
                    Step::Keep => return None,
                };
                Some((LayoutKey::Array(Box::new(key)), real))
            }
            Layout::Record {
                name: base_name,
                tuple: is_tuple,
                fields,
            } => {
                visiting.push(ty);
                let fields = self.layout_key_fields(ctx, &fields, objects, visiting);
                visiting.pop();
                let specialized_fields = fields?;
                let key = LayoutKey::Record {
                    base_name,
                    is_tuple,
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
            match self.heap.step(ctx, objects, Some(index), ty)? {
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
                let Some(slots) = self.heap.slots(objects, field) else {
                    return Ok(());
                };
                if slots
                    .iter()
                    .any(|&slot| self.heap.cells[slot].domain.needs_lossless_64())
                    && numeric_type_for(ctx, child).is_some()
                {
                    let message = name.map_or_else(
                        || "array element with Fact::Numeric has f64 carrier".to_string(),
                        |name| format!("record field `{name}` with Fact::Numeric has f64 carrier"),
                    );
                    return pliron::verify_err!(
                        loc.clone(),
                        crate::facts::rewrite::LosslessRepresentationErr(message)
                    );
                }
                if aggregate(ctx, child) {
                    let nested = self.heap.layout_objects(&slots);
                    self.verify_objects_layout_lossless(ctx, loc, child, &nested, visiting)?;
                }
            }
            Ok(())
        });
        visiting.remove(&ty);
        verified
    }
}

/// The array or record shape of an aggregate type, copied out of the type storage because
/// specializing instantiates types while it is walked.
enum Layout {
    Array(TypeHandle),
    Record {
        name: Identifier,
        tuple: bool,
        fields: Vec<(String, TypeHandle)>,
    },
}

impl Layout {
    /// `None` for other types and for opaque records, which have nothing to specialize.
    fn of(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let ty = ty.deref(ctx);
        if let Some(array) = ty.downcast_ref::<ArrayType>() {
            return Some(Self::Array(array.element()));
        }
        let record = ty.downcast_ref::<RecordType>()?;
        Some(Self::Record {
            name: record.name().clone(),
            tuple: record.is_tuple(),
            fields: record.fields().ok()?.to_vec(),
        })
    }

    /// `(field index, field name, type)` of each child; an array's one child is its element.
    fn children(&self) -> Vec<(Option<usize>, Option<&str>, TypeHandle)> {
        match self {
            Self::Array(element) => vec![(None, None, *element)],
            Self::Record { fields, .. } => fields
                .iter()
                .enumerate()
                .map(|(index, (name, ty))| (Some(index), Some(name.as_str()), *ty))
                .collect(),
        }
    }
}

/// How the slots holding one child of every object in a group specialize.
enum Step {
    /// Every slot needs a lossless carrier and the child is an `f64` one: its numeric type.
    Numeric(TypeHandle),
    /// The child is an aggregate, decided by the objects its slots may hold.
    Nested(BTreeSet<usize>),
    Keep,
}

impl HeapFacts {
    /// The slot each of `objects` holds at record `field` (array elements for `None`), or
    /// `None` when an object has another shape.
    fn slots(&self, objects: &BTreeSet<usize>, field: Option<usize>) -> Option<Vec<usize>> {
        objects
            .iter()
            .map(|&object| match (&self.objects[object].contents, field) {
                (Contents::Array { element, .. }, None) => Some(*element),
                (Contents::Record(slots), Some(index)) => slots.get(index).copied(),
                _ => None,
            })
            .collect()
    }

    /// The objects any of `slots` may lay out.
    fn layout_objects(&self, slots: &[usize]) -> BTreeSet<usize> {
        slots
            .iter()
            .flat_map(|&slot| self.cells[slot].layout_objects.iter().copied())
            .collect()
    }

    /// How child `field` of type `ty` specializes across `objects`; `None` for a shape mismatch.
    fn step(
        &self,
        ctx: &Context,
        objects: &BTreeSet<usize>,
        field: Option<usize>,
        ty: TypeHandle,
    ) -> Option<Step> {
        let slots = self.slots(objects, field)?;
        let lossless = slots
            .iter()
            .all(|&slot| self.cells[slot].domain.needs_lossless_64());
        Some(match numeric_type_for(ctx, ty) {
            Some(numeric) if lossless => Step::Numeric(numeric),
            _ if aggregate(ctx, ty) => Step::Nested(self.layout_objects(&slots)),
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
