//! Physical aggregate layouts from converged slot facts.
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
        if ty.deref(ctx).is::<crate::SlotType>()
            && let Some(name) = global(ctx, value)
            && let Some(&cell) = self.heap.globals.get(&name)
        {
            let inner = self.heap.cells[cell].ty;
            return Ok(self
                .specialized_cell_type(ctx, inner, cell)?
                .map(|layout| crate::SlotType::get(ctx, layout).into()));
        }
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
            if let Some(promise) = ty.deref(ctx).downcast_ref::<PromiseType>() {
                return Ok(numeric_type_for(ctx, promise.value())
                    .map(|payload| PromiseType::get(ctx, payload).into()));
            }
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
        self.cell_layout(ctx, ty, cell, LayoutRule::Numeric, &mut |_, _| true)
    }

    pub(super) fn cell_layout(
        &self,
        ctx: &Context,
        ty: TypeHandle,
        cell: usize,
        rule: LayoutRule,
        nominal: &mut dyn FnMut(TypeHandle, TypeHandle) -> bool,
    ) -> Result<Option<TypeHandle>> {
        let nodes = LayoutPlanner::nodes(ctx, &self.heap, ty, cell, rule);
        LayoutPlan::new(nodes)
            .map(|plan| plan.materialize(ctx, 0, &mut HashMap::new(), nominal))
            .transpose()
    }

    pub(in crate::facts) fn verify_cell_layout_lossless(
        &self,
        ctx: &Context,
        loc: &pliron::location::Location,
        ty: TypeHandle,
        cell: usize,
    ) -> pliron::result::Result<()> {
        let nodes = LayoutPlanner::nodes(ctx, &self.heap, ty, cell, LayoutRule::VerifyNumeric);
        for node in &nodes {
            for (index, &child) in node.children().iter().enumerate() {
                if !matches!(nodes[child], LayoutNode::Replace(_)) {
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LayoutRule {
    Numeric,
    VerifyNumeric,
    Source,
}

/// One heap walk supplies both the specialization graph and the lossless verifier.
struct LayoutPlan {
    nodes: Vec<LayoutNode>,
    changed: Vec<bool>,
}

enum LayoutNode {
    Keep(TypeHandle),
    Replace(TypeHandle),
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
            Self::Keep(_) | Self::Replace(_) => &[],
        }
    }
}

struct LayoutPlanner<'a> {
    ctx: &'a Context,
    heap: &'a HeapFacts,
    rule: LayoutRule,
    visiting: HashMap<TypeHandle, usize>,
    nodes: Vec<LayoutNode>,
}

impl LayoutPlan {
    fn new(nodes: Vec<LayoutNode>) -> Option<Self> {
        let mut pending: Vec<_> = nodes
            .iter()
            .enumerate()
            .filter_map(|(id, node)| matches!(node, LayoutNode::Replace(_)).then_some(id))
            .collect();
        if pending.is_empty() {
            return None;
        }
        let mut changed = vec![false; nodes.len()];
        let mut dependents = vec![Vec::new(); nodes.len()];
        for (parent, node) in nodes.iter().enumerate() {
            for &child in node.children() {
                dependents[child].push(parent);
            }
        }
        while let Some(node) = pending.pop() {
            if !std::mem::replace(&mut changed[node], true) {
                pending.extend(dependents[node].iter().copied());
            }
        }
        Some(Self { nodes, changed })
    }

    fn materialize(
        &self,
        ctx: &Context,
        node: usize,
        constructed: &mut HashMap<usize, (TypeHandle, bool)>,
        nominal: &mut dyn FnMut(TypeHandle, TypeHandle) -> bool,
    ) -> Result<TypeHandle> {
        if let Some((ty, referenced)) = constructed.get_mut(&node) {
            *referenced = true;
            return Ok(*ty);
        }
        match &self.nodes[node] {
            LayoutNode::Keep(ty) | LayoutNode::Replace(ty) => Ok(*ty),
            LayoutNode::Optional { absence, inner } => {
                let inner = self.materialize(ctx, *inner, constructed, nominal)?;
                Ok(crate::types::OptionalType::get_with(ctx, inner, *absence).into())
            }
            LayoutNode::Array(element) => {
                let element = self.materialize(ctx, *element, constructed, nominal)?;
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
                constructed.insert(node, (reserved, false));
                let mut fields = ty.deref(ctx).fields()?.to_vec();
                for ((_, ty), &child) in fields.iter_mut().zip(children) {
                    *ty = self.materialize(ctx, child, constructed, nominal)?;
                }
                let layout = if nominal((*ty).into(), reserved) || constructed[&node].1 {
                    RecordType::get_kind(ctx, name, Some(fields), kind)?
                } else {
                    RecordType::structural(ctx, kind, fields)?
                };
                constructed.insert(node, (layout.into(), false));
                Ok(layout.into())
            }
        }
    }

    fn write_key(&self, ctx: &Context, node: usize, path: &mut Vec<usize>, out: &mut String) {
        if let Some(depth) = path.iter().rev().position(|&parent| parent == node) {
            let _ = write!(out, "S{depth}_");
            return;
        }
        match &self.nodes[node] {
            LayoutNode::Replace(ty) => out.push(if crate::is_byte_input(ctx, payload(ctx, *ty)) {
                'B'
            } else {
                'N'
            }),
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
    fn nodes(
        ctx: &Context,
        heap: &HeapFacts,
        ty: TypeHandle,
        cell: usize,
        rule: LayoutRule,
    ) -> Vec<LayoutNode> {
        let mut planner = LayoutPlanner {
            ctx,
            heap,
            rule,
            visiting: HashMap::new(),
            nodes: Vec::new(),
        };
        planner.visit(ty, cell);
        planner.nodes
    }

    fn visit(&mut self, ty: TypeHandle, cell: usize) -> usize {
        let objects = &self.heap.cells[cell].layout_objects;
        if let Some(&node) = self.visiting.get(&ty) {
            return node;
        }
        let node = self.nodes.len();
        self.nodes.push(LayoutNode::Keep(ty));
        if self.rule == LayoutRule::Source
            && self.heap.cells[cell].facts.source_tag
            && let Some(tagged) = tagged_type(self.ctx, ty)
        {
            self.nodes[node] = LayoutNode::Replace(tagged);
            return node;
        }
        if objects.is_empty() {
            return node;
        }
        self.visiting.insert(ty, node);
        let layout = if let Some((inner, absence)) = crate::optional::optional_parts(self.ctx, ty) {
            Some(LayoutNode::Optional {
                absence,
                inner: self.visit(inner, cell),
            })
        } else if let Some(array) = crate::typed::<ArrayType>(self.ctx, ty) {
            let element = array.deref(self.ctx).element();
            self.child(objects, None, element).map(LayoutNode::Array)
        } else if let Some(record) = crate::typed::<RecordType>(self.ctx, ty) {
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
        let mut child = None;
        let mut lossless = self.rule == LayoutRule::Numeric;
        for &object in objects {
            let slot = match (&self.heap.objects[object].contents, field) {
                (Contents::Array { element, .. }, None) => *element,
                (Contents::Record(slots), Some(index)) => *slots.get(index)?,
                _ => return None,
            };
            let cell = &self.heap.cells[slot];
            // Sibling slots already agree on every nested layout when the solver converges.
            debug_assert!(child.is_none_or(
                |other: usize| self.heap.cells[other].layout_objects == cell.layout_objects
            ));
            child = Some(slot);
            let required = cell.facts.domain.needs_lossless_64();
            match self.rule {
                LayoutRule::Numeric => lossless &= required,
                LayoutRule::VerifyNumeric => lossless |= required,
                LayoutRule::Source => (),
            }
        }
        if self.rule == LayoutRule::Source || aggregate(self.ctx, ty) {
            return Some(self.visit(ty, child?));
        }
        let node = if lossless && let Some(numeric) = numeric_type_for(self.ctx, ty) {
            LayoutNode::Replace(numeric)
        } else {
            LayoutNode::Keep(ty)
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

pub(super) fn tagged_type(ctx: &Context, ty: TypeHandle) -> Option<TypeHandle> {
    let value = crate::present_type(ctx, ty).deref(ctx);
    (value.is::<crate::StringType>() || value.is::<crate::BytesType>()).then(|| {
        let tagged = crate::byte_input(ctx);
        crate::optional::absence(ctx, ty).map_or(tagged, |absence| {
            crate::types::OptionalType::get_with(ctx, tagged, absence).into()
        })
    })
}
