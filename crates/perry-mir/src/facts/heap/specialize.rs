//! Numeric layout specialization of aggregate types from converged slot domains.
use super::*;
use pliron::identifier::Legaliser;
use std::fmt::Write;

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
        if res.facts.domain == Fact::Numeric
            || (res.facts.domain.needs_lossless_64()
                && ty.deref(ctx).is::<crate::types::OptionalType>())
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
        let plan = LayoutPlan::new(ctx, &self.heap, ty, cell, Lossless::All);
        if !plan.changed[0] {
            return Ok(None);
        }
        plan.materialize(ctx, 0, &mut HashMap::new()).map(Some)
    }

    pub(in crate::facts) fn verify_cell_layout_lossless(
        &self,
        ctx: &Context,
        loc: &pliron::location::Location,
        ty: TypeHandle,
        cell: usize,
    ) -> pliron::result::Result<()> {
        let plan = LayoutPlan::new(ctx, &self.heap, ty, cell, Lossless::Any);
        for node in &plan.nodes {
            for (index, &child) in node.children().iter().enumerate() {
                if !matches!(plan.nodes[child], LayoutNode::Numeric(_)) {
                    continue;
                }
                let message = match node {
                    LayoutNode::Record { ty, .. } => format!(
                        "record field `{}` with Fact::Numeric has f64 carrier",
                        ty.deref(ctx).fields()?[index].0
                    ),
                    _ => "array element with Fact::Numeric has f64 carrier".to_string(),
                };
                return pliron::verify_err!(
                    loc.clone(),
                    crate::facts::rewrite::LosslessRepresentationErr(message)
                );
            }
        }
        Ok(())
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
            let needs_lossless = cell.facts.domain.needs_lossless_64();
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

/// One heap walk supplies both the specialization graph and the lossless verifier.
struct LayoutPlan {
    nodes: Vec<LayoutNode>,
    changed: Vec<bool>,
}

enum LayoutNode {
    Keep(TypeHandle),
    Numeric(TypeHandle),
    Optional {
        absence: crate::Absence,
        inner: usize,
    },
    Array(usize),
    Record {
        ty: TypedHandle<RecordType>,
        fields: Vec<usize>,
    },
}

impl LayoutNode {
    fn children(&self) -> &[usize] {
        match self {
            Self::Optional { inner, .. } | Self::Array(inner) => std::slice::from_ref(inner),
            Self::Record { fields, .. } => fields,
            Self::Keep(_) | Self::Numeric(_) => &[],
        }
    }
}

struct LayoutPlanner<'a> {
    ctx: &'a Context,
    heap: &'a HeapFacts,
    lossless: Lossless,
    visiting: HashMap<TypeHandle, usize>,
    nodes: Vec<LayoutNode>,
}

impl LayoutPlan {
    fn new(
        ctx: &Context,
        heap: &HeapFacts,
        ty: TypeHandle,
        cell: usize,
        lossless: Lossless,
    ) -> Self {
        let mut planner = LayoutPlanner {
            ctx,
            heap,
            lossless,
            visiting: HashMap::new(),
            nodes: Vec::new(),
        };
        planner.visit(ty, &heap.cells[cell].layout_objects);
        let nodes = planner.nodes;
        let mut changed = vec![false; nodes.len()];
        let mut dependents = vec![Vec::new(); nodes.len()];
        let mut pending = Vec::new();
        for (parent, node) in nodes.iter().enumerate() {
            if matches!(node, LayoutNode::Numeric(_)) {
                pending.push(parent);
            }
            for &child in node.children() {
                dependents[child].push(parent);
            }
        }
        while let Some(node) = pending.pop() {
            if !std::mem::replace(&mut changed[node], true) {
                pending.extend(dependents[node].iter().copied());
            }
        }
        Self { nodes, changed }
    }

    fn materialize(
        &self,
        ctx: &Context,
        node: usize,
        constructed: &mut HashMap<usize, TypeHandle>,
    ) -> Result<TypeHandle> {
        if let Some(&ty) = constructed.get(&node) {
            return Ok(ty);
        }
        match &self.nodes[node] {
            LayoutNode::Keep(ty) | LayoutNode::Numeric(ty) => Ok(*ty),
            LayoutNode::Optional { absence, inner } => {
                let inner = self.materialize(ctx, *inner, constructed)?;
                Ok(crate::types::OptionalType::get_with(ctx, inner, *absence).into())
            }
            LayoutNode::Array(element) => {
                let element = self.materialize(ctx, *element, constructed)?;
                Ok(ArrayType::get(ctx, element).into())
            }
            LayoutNode::Record {
                ty,
                fields: children,
            } => {
                if !self.changed[node] {
                    return Ok((*ty).into());
                }
                let mut encoded = String::from("perry_layout_");
                self.write_key(ctx, node, &mut Vec::new(), &mut encoded);
                let name = Legaliser::default().legalise(&encoded);
                let kind = ty.deref(ctx).kind();
                let reserved: TypeHandle =
                    RecordType::get_kind(ctx, name.clone(), None, kind)?.into();
                constructed.insert(node, reserved);
                let mut fields = ty.deref(ctx).fields()?.to_vec();
                for ((_, ty), &child) in fields.iter_mut().zip(children) {
                    *ty = self.materialize(ctx, child, constructed)?;
                }
                Ok(RecordType::get_kind(ctx, name, Some(fields), kind)?.into())
            }
        }
    }

    fn write_key(&self, ctx: &Context, node: usize, path: &mut Vec<usize>, out: &mut String) {
        if let Some(depth) = path.iter().rev().position(|&parent| parent == node) {
            let _ = write!(out, "S{depth}_");
            return;
        }
        match &self.nodes[node] {
            LayoutNode::Numeric(_) => out.push('N'),
            LayoutNode::Optional { absence, inner } => {
                let _ = write!(out, "O{}_", *absence as u8);
                self.write_key(ctx, *inner, path, out);
            }
            LayoutNode::Array(element) => {
                out.push('A');
                self.write_key(ctx, *element, path, out);
            }
            LayoutNode::Record { ty, fields } => {
                let record = ty.deref(ctx);
                out.push(if record.is_tuple() { 'T' } else { 'R' });
                write_ident(out, record.name().as_ref());
                let _ = write!(
                    out,
                    "{}_",
                    fields.iter().filter(|&&c| self.changed[c]).count()
                );
                path.push(node);
                for (index, &child) in fields.iter().enumerate() {
                    if self.changed[child] {
                        let _ = write!(out, "F{index}_");
                        self.write_key(ctx, child, path, out);
                    }
                }
                path.pop();
            }
            LayoutNode::Keep(_) => {}
        }
    }
}

impl LayoutPlanner<'_> {
    fn visit(&mut self, ty: TypeHandle, objects: &BTreeSet<usize>) -> usize {
        if let Some(&node) = self.visiting.get(&ty) {
            return node;
        }
        let node = self.nodes.len();
        self.nodes.push(LayoutNode::Keep(ty));
        if objects.is_empty() {
            return node;
        }
        self.visiting.insert(ty, node);
        let layout = if let Some((inner, absence)) = crate::optional::optional_parts(self.ctx, ty) {
            Some(LayoutNode::Optional {
                absence,
                inner: self.visit(inner, objects),
            })
        } else if let Ok(array) = TypedHandle::<ArrayType>::from_handle(ty, self.ctx) {
            let element = array.deref(self.ctx).element();
            self.child(objects, None, element).map(LayoutNode::Array)
        } else if let Ok(record) = TypedHandle::<RecordType>::from_handle(ty, self.ctx) {
            record
                .deref(self.ctx)
                .fields()
                .ok()
                .and_then(|fields| {
                    fields
                        .iter()
                        .enumerate()
                        .map(|(i, (_, ty))| self.child(objects, Some(i), *ty))
                        .collect::<Option<Vec<_>>>()
                })
                .map(|fields| LayoutNode::Record { ty: record, fields })
        } else {
            None
        };
        self.visiting.remove(&ty);
        match layout {
            Some(layout) => self.nodes[node] = layout,
            None => self.nodes.truncate(node + 1),
        }
        node
    }

    fn child(
        &mut self,
        objects: &BTreeSet<usize>,
        field: Option<usize>,
        ty: TypeHandle,
    ) -> Option<usize> {
        let step = self
            .heap
            .step(self.ctx, objects, field, ty, self.lossless)?;
        let node = match step {
            Step::Numeric(ty) => LayoutNode::Numeric(ty),
            Step::Nested(nested) => return Some(self.visit(ty, &nested)),
            Step::Keep => LayoutNode::Keep(ty),
        };
        let index = self.nodes.len();
        self.nodes.push(node);
        Some(index)
    }
}

fn write_ident(out: &mut String, ident: &str) {
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
