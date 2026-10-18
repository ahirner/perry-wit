//! Alias, JSON-residue, and intervening-mutation queries over converged `HeapFacts`.
use pliron::{
    basic_block::BasicBlock,
    linked_list::{ContainsLinkedList, LinkedList},
};

use super::*;
use crate::{exceptions::RaiseOp, foreign::ForeignOp, is_any, locals::StoreOp};

fn type_may_contain_heap_aggregate(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::present_type(ctx, ty);
    let inner_ref = inner.deref(ctx);
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
/// instead hold an untracked aggregate. `visited` ends a wrapper cycle (possible only in
/// unreachable code) as untracked rather than looping.
fn root_heap_cells(ctx: &Context, state: &State, value: Value) -> (Vec<usize>, bool) {
    let mut visited = HashSet::new();
    let mut current = value;
    loop {
        if let Some(&cell) = state.heap_values.get(&current) {
            return (vec![cell], false);
        }
        let wrapper = current.defining_op().filter(|&op| {
            (Operation::get_op::<WrapOp>(op, ctx).is_some()
                || Operation::get_op::<UnwrapOp>(op, ctx).is_some()
                || Operation::get_op::<OptionalCastOp>(op, ctx).is_some()
                || Operation::get_op::<crate::variants::VariantNewOp>(op, ctx).is_some())
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

impl ProgramFacts {
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
        let (root_cells, untracked) = root_heap_cells(ctx, state, source_value);
        let (reachable_objects, open_or_summarized) =
            collect_reachable_heap(&self.heap, &root_cells, untracked);
        let has_heap = !reachable_objects.is_empty() || open_or_summarized;
        // Whether `value` may share an object with the source, or both are open.
        let aliases = |value: Value| {
            let (cells, untracked) = root_heap_cells(ctx, state, value);
            let (objects, open) = collect_reachable_heap(&self.heap, &cells, untracked);
            !reachable_objects.is_disjoint(&objects) || open_or_summarized && open
        };

        let Some(stringify_block) = stringify_op.deref(ctx).get_parent_block() else {
            return true;
        };
        let Some(consumer_block) = consumer_op.deref(ctx).get_parent_block() else {
            return true;
        };

        let mut intervening_ops = Vec::new();
        if stringify_block == consumer_block {
            let mut cursor = stringify_op.deref(ctx).get_next();
            let mut found_consumer = false;
            while let Some(op) = cursor {
                if op == consumer_op {
                    found_consumer = true;
                    break;
                }
                intervening_ops.push(op);
                cursor = op.deref(ctx).get_next();
            }
            if !found_consumer {
                return true;
            }
        } else {
            let successors = |block: Ptr<BasicBlock>| -> Vec<Ptr<BasicBlock>> {
                let terminator = block.deref(ctx).get_tail();
                terminator.map_or_else(Vec::new, |term| term.deref(ctx).successors().collect())
            };
            // Blocks on paths from the stringify towards the consumer, which ends them.
            let forward = crate::reachable(successors(stringify_block), |block| {
                if block == consumer_block {
                    Vec::new()
                } else {
                    successors(block)
                }
            });
            if !forward.contains(&consumer_block) {
                return true;
            }
            let mut cursor = stringify_op.deref(ctx).get_next();
            while let Some(op) = cursor {
                intervening_ops.push(op);
                cursor = op.deref(ctx).get_next();
            }
            for &block in &forward {
                if block == consumer_block {
                    continue;
                }
                let mut op_cur = block.deref(ctx).get_head();
                while let Some(op) = op_cur {
                    if op != stringify_op && op != consumer_op {
                        intervening_ops.push(op);
                    }
                    op_cur = op.deref(ctx).get_next();
                }
            }
            let mut op_cur = consumer_block.deref(ctx).get_head();
            while let Some(op) = op_cur {
                if op == consumer_op {
                    break;
                }
                intervening_ops.push(op);
                op_cur = op.deref(ctx).get_next();
            }
        }

        let unwrap_may_throw = unwrap_op.is_some()
            && (crate::optional::absence(ctx, source_value.get_type(ctx)).is_some()
                || source_value
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<crate::types::JsonType>());
        let mut seen_intermediate_parse = false;

        for op in intervening_ops {
            if Some(op) == unwrap_op {
                continue;
            }
            if Some(op) == intermediate_parse_op {
                seen_intermediate_parse = true;
                continue;
            }
            if Operation::get_op::<crate::exceptions::CaughtOp>(op, ctx).is_some() {
                continue;
            }

            let mutates = || {
                if is_any::<(
                    FieldSetOp,
                    ArraySetOp,
                    ArrayPushOp,
                    ArrayExtendOp,
                    ArrayTruncateOp,
                )>(ctx, op)
                {
                    aliases(op.deref(ctx).get_operand(0))
                } else if Operation::is_op::<StoreOp>(op, ctx) {
                    open_or_summarized
                } else if is_any::<(CallOp, AsyncCallOp)>(ctx, op) {
                    op.deref(ctx).operands().any(aliases)
                        || open_or_summarized
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
                is_any::<(ForeignOp, CallOp, AsyncCallOp, AwaitOp, TimerOp, RaiseOp)>(ctx, op)
                    || Operation::is_op::<crate::intrinsics::IntrinsicOp>(op, ctx)
            };
            if has_heap && mutates() || (unwrap_may_throw || seen_intermediate_parse) && reorders()
            {
                return true;
            }
        }
        false
    }
}
