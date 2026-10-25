//! Alias, JSON-residue, and intervening-mutation queries over converged `HeapFacts`.
use super::*;
use crate::effects::Mutation;
use std::collections::HashSet;

fn type_may_contain_heap_aggregate(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::present_type(ctx, ty);
    let inner_ref = inner.deref(ctx);
    if let Some(union) = inner_ref.downcast_ref::<crate::ValueUnionType>() {
        return type_may_contain_heap_aggregate(ctx, union.variants());
    }
    if inner_ref.is::<RecordType>()
        || inner_ref.is::<ArrayType>()
        || inner_ref.is::<crate::types::JsonType>()
    {
        return true;
    }
    if let Some(variant) = inner_ref.downcast_ref::<crate::types::VariantType>() {
        return variant
            .cases()
            .iter()
            .filter_map(|(_, payload)| *payload)
            .any(|payload| type_may_contain_heap_aggregate(ctx, payload));
    }
    false
}

/// The heap cell `value` names, looking through single-operand wrappers, and whether it may
/// hold an untracked aggregate. `visited` terminates wrapper cycles in unreachable code as
/// untracked.
fn root_heap_cells(ctx: &Context, state: &State, value: Value) -> (Vec<usize>, bool) {
    let mut visited = HashSet::new();
    let mut current = value;
    loop {
        if let Some(&cell) = state.heap_values.get(&current) {
            return (vec![cell], false);
        }
        let wrapper = current.defining_op().filter(|&op| {
            crate::is_any::<(
                WrapOp,
                UnwrapOp,
                OptionalCastOp,
                crate::variants::VariantNewOp,
            )>(ctx, op)
                && op.deref(ctx).get_num_operands() == 1
                && visited.insert(op)
        });
        match wrapper {
            Some(op) => current = op.deref(ctx).get_operand(0),
            None => {
                let untracked = type_may_contain_heap_aggregate(ctx, current.get_type(ctx));
                return (Vec::new(), untracked);
            }
        }
    }
}

/// The objects reachable from `root_cells`, and whether any of them may hold unmodelled
/// contents: an untracked root, an empty or summarized cell, or an open object.
fn collect_reachable_heap(
    heap: &HeapFacts,
    root_cells: &[usize],
    untracked: bool,
) -> (BTreeSet<usize>, bool) {
    let cells = crate::reachable(root_cells.iter().copied(), move |cell| {
        heap.cells[cell]
            .objects
            .iter()
            .flat_map(move |&object| heap.objects[object].contents.slots().iter().copied())
    });
    let objects: BTreeSet<usize> = cells
        .iter()
        .flat_map(|&cell| heap.cells[cell].objects.iter().copied())
        .collect();
    let open = untracked
        || root_cells
            .iter()
            .any(|&cell| heap.cells[cell].objects.is_empty())
        || cells.iter().any(|&cell| heap.cells[cell].summarized)
        || objects.iter().any(|&object| heap.objects[object].open);
    (objects, open)
}

struct Aliases<'a> {
    ctx: &'a Context,
    heap: &'a HeapFacts,
    objects: BTreeSet<usize>,
    open: bool,
}

impl<'a> Aliases<'a> {
    fn new(ctx: &'a Context, heap: &'a HeapFacts, state: &State, value: Value) -> Self {
        let (cells, untracked) = root_heap_cells(ctx, state, value);
        let (objects, open) = collect_reachable_heap(heap, &cells, untracked);
        Self {
            ctx,
            heap,
            objects,
            open,
        }
    }

    fn contains(&self, state: &State, value: Value) -> bool {
        let (cells, untracked) = root_heap_cells(self.ctx, state, value);
        let (objects, open) = collect_reachable_heap(self.heap, &cells, untracked);
        !self.objects.is_disjoint(&objects) || self.open && open
    }
}

impl ProgramFacts {
    pub(crate) fn array_length_invalidation<'a>(
        &'a self,
        ctx: &'a Context,
        function: &FuncOp,
        array: Value,
    ) -> impl FnMut(Ptr<Operation>) -> bool + 'a {
        let state = self
            .generic
            .get(&function.get_operation())
            .map(|&id| &self.states[id]);
        let aliases = state.map(|state| Aliases::new(ctx, &self.heap, state, array));
        let mut calls = HashMap::new();
        move |op| {
            let Some((state, aliases)) = state.zip(aliases.as_ref()) else {
                return true;
            };
            let kind = OperationKind::classify(ctx, op);
            if kind == OperationKind::Mutate(Mutation::Truncate) {
                return aliases.contains(state, op.deref(ctx).get_operand(0));
            }
            if let Some(callee) = crate::calls::callee(ctx, op) {
                let Some(id) = self.generic_context(&callee) else {
                    return true;
                };
                return *calls
                    .entry(id)
                    .or_insert_with(|| self.call_shrinks_array(ctx, id, aliases));
            }
            matches!(kind, OperationKind::Foreign(_) | OperationKind::Await)
        }
    }

    fn call_shrinks_array(&self, ctx: &Context, root: usize, aliases: &Aliases<'_>) -> bool {
        let mut pending = vec![root];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let state = &self.states[id];
            let effects = state.summary.effects;
            if effects.may_suspend || effects.may_reenter {
                return true;
            }
            if !effects.may_mutate_state() || !effects.shrinks_arrays {
                continue;
            }
            for constraint in &self.bodies[&self.keys[id].function].constraints {
                match *constraint {
                    Constraint::Op {
                        op,
                        kind: OperationKind::Mutate(Mutation::Truncate),
                    } if aliases.contains(state, op.deref(ctx).get_operand(0)) => return true,
                    Constraint::Op {
                        op,
                        kind: OperationKind::Call { .. },
                    } => {
                        let Some(child) = crate::calls::callee(ctx, op)
                            .and_then(|name| self.generic_context(&name))
                        else {
                            return true;
                        };
                        pending.push(child);
                    }
                    _ => {}
                }
            }
        }
        false
    }

    /// Whether `value` in `function` may originate from an open/unmodelled JSON source
    /// that could carry extra unvalidated JSON fields beyond its static MIR type.
    pub(crate) fn may_have_json_residue(
        &self,
        ctx: &Context,
        function: &FuncOp,
        value: Value,
    ) -> bool {
        let Some(&id) = self.generic.get(&function.get_operation()) else {
            return true;
        };
        let state = &self.states[id];
        let (root_cells, untracked) = root_heap_cells(ctx, state, value);
        let (_, open_or_summarized) = collect_reachable_heap(&self.heap, &root_cells, untracked);
        open_or_summarized
    }

    /// Whether any operation on a path from `stringify_op` to `consumer_op` may mutate
    /// `source_value` (using object-identity alias facts) or observe altered exception order.
    pub(crate) fn may_prevent_json_copy_between(
        &self,
        ctx: &Context,
        function: &FuncOp,
        source_value: Value,
        stringify_op: Ptr<Operation>,
        intermediate_ops: (Option<Ptr<Operation>>, Option<Ptr<Operation>>),
        consumer_op: Ptr<Operation>,
    ) -> bool {
        let (unwrap_op, intermediate_parse_op) = intermediate_ops;
        let Some(&id) = self.generic.get(&function.get_operation()) else {
            return true;
        };
        let state = &self.states[id];
        let source = Aliases::new(ctx, &self.heap, state, source_value);
        let has_heap = !source.objects.is_empty() || source.open;
        let aliases = |value| source.contains(state, value);

        let unwrap_may_throw = unwrap_op.is_some()
            && (crate::optional::absence(ctx, source_value.get_type(ctx)).is_some()
                || source_value
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<crate::types::JsonType>());
        !super::super::paths::preserves(
            ctx,
            stringify_op,
            consumer_op,
            intermediate_parse_op.is_some(),
            |op, after_parse| {
                if Some(op) == unwrap_op {
                    return Some(after_parse);
                }
                if Some(op) == intermediate_parse_op {
                    return Some(false);
                }
                if Operation::get_op::<crate::exceptions::CaughtOp>(op, ctx).is_some() {
                    return Some(after_parse);
                }

                let kind = OperationKind::classify(ctx, op);
                let mutates = || {
                    if let OperationKind::Mutate(mutation) = kind {
                        if mutation == Mutation::Local {
                            source.open
                        } else {
                            aliases(op.deref(ctx).get_operand(0))
                        }
                    } else if matches!(kind, OperationKind::Call { .. }) {
                        op.deref(ctx).operands().any(aliases)
                            || source.open
                                && crate::calls::callee(ctx, op).is_none_or(|callee| {
                                    self.effects(&callee)
                                        .is_none_or(|summary| summary.effects.may_mutate_state())
                                })
                    } else {
                        false
                    }
                };
                // Moving a throwing source past another exception source reorders exceptions.
                let reorders = || {
                    matches!(
                        kind,
                        OperationKind::Foreign(_)
                            | OperationKind::Call { .. }
                            | OperationKind::Await
                            | OperationKind::Timer
                            | OperationKind::Raise
                            | OperationKind::Intrinsic(_)
                    )
                };
                (!(has_heap && mutates() || (unwrap_may_throw || after_parse) && reorders()))
                    .then_some(after_parse)
            },
        )
    }
}
