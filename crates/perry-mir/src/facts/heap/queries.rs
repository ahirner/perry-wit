//! Alias, JSON-residue, and intervening-mutation queries over converged `HeapFacts`.
use pliron::linked_list::{ContainsLinkedList, LinkedList};

use super::*;

fn type_may_contain_heap_aggregate(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
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

fn root_heap_cells(ctx: &Context, state: &State, value: Value) -> (Vec<usize>, bool) {
    let mut cells = Vec::new();
    let mut untracked = false;
    let mut current = value;
    for _ in 0..16 {
        if let Some(&cell) = state.heap_values.get(&current) {
            cells.push(cell);
            break;
        }
        let Some(op) = current.defining_op() else {
            if type_may_contain_heap_aggregate(ctx, current.get_type(ctx)) {
                untracked = true;
            }
            break;
        };
        if (Operation::get_op::<WrapOp>(op, ctx).is_some()
            || Operation::get_op::<UnwrapOp>(op, ctx).is_some()
            || Operation::get_op::<OptionalCastOp>(op, ctx).is_some()
            || Operation::get_op::<crate::variants::VariantNewOp>(op, ctx).is_some())
            && op.deref(ctx).get_num_operands() == 1
        {
            current = op.deref(ctx).get_operand(0);
            continue;
        }
        if type_may_contain_heap_aggregate(ctx, current.get_type(ctx)) {
            untracked = true;
        }
        break;
    }
    (cells, untracked)
}

fn collect_reachable_heap(
    heap: &HeapFacts,
    root_cells: &[usize],
    untracked: bool,
) -> (BTreeSet<usize>, bool) {
    let mut visited_cells = HashSet::new();
    let mut reachable_objects = BTreeSet::new();
    let mut open_or_summarized = untracked;
    for &cell_id in root_cells {
        if heap
            .cells
            .get(cell_id)
            .is_none_or(|cell| cell.objects.is_empty())
        {
            open_or_summarized = true;
        }
    }
    let mut work: VecDeque<usize> = root_cells.iter().copied().collect();
    while let Some(cell_id) = work.pop_front() {
        if !visited_cells.insert(cell_id) {
            continue;
        }
        let Some(cell) = heap.cells.get(cell_id) else {
            open_or_summarized = true;
            continue;
        };
        if cell.summarized {
            open_or_summarized = true;
        }
        for &obj_id in &cell.objects {
            reachable_objects.insert(obj_id);
            let Some(obj) = heap.objects.get(obj_id) else {
                open_or_summarized = true;
                continue;
            };
            if obj.open {
                open_or_summarized = true;
            }
            for &slot_cell in obj.contents.slots() {
                work.push_back(slot_cell);
            }
        }
    }
    (reachable_objects, open_or_summarized)
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
        let Some(&id) = self.generic.get(&function.get_symbol_name(ctx)) else {
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
        let Some(&id) = self.generic.get(&function.get_symbol_name(ctx)) else {
            return true;
        };
        let state = &self.states[id];
        let (root_cells, untracked) = root_heap_cells(ctx, state, source_value);
        let (reachable_objects, open_or_summarized) =
            collect_reachable_heap(&self.heap, &root_cells, untracked);
        let has_heap = !reachable_objects.is_empty() || open_or_summarized;

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
            let mut forward = HashSet::new();
            let mut queue = VecDeque::new();
            if let Some(term) = stringify_block.deref(ctx).get_tail() {
                for idx in 0..term.deref(ctx).get_num_successors() {
                    let succ = term.deref(ctx).get_successor(idx);
                    if forward.insert(succ) {
                        queue.push_back(succ);
                    }
                }
            }
            while let Some(block) = queue.pop_front() {
                if block == consumer_block {
                    continue;
                }
                if let Some(term) = block.deref(ctx).get_tail() {
                    for idx in 0..term.deref(ctx).get_num_successors() {
                        let succ = term.deref(ctx).get_successor(idx);
                        if forward.insert(succ) {
                            queue.push_back(succ);
                        }
                    }
                }
            }
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

            if has_heap {
                if Operation::get_op::<FieldSetOp>(op, ctx).is_some()
                    || Operation::get_op::<ArraySetOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayExtendOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayTruncateOp>(op, ctx).is_some()
                {
                    let target = op.deref(ctx).get_operand(0);
                    let (target_cells, target_untracked) = root_heap_cells(ctx, state, target);
                    let (target_objs, target_open) =
                        collect_reachable_heap(&self.heap, &target_cells, target_untracked);
                    if !reachable_objects.is_disjoint(&target_objs)
                        || (open_or_summarized && target_open)
                    {
                        return true;
                    }
                } else if open_or_summarized
                    && Operation::get_op::<crate::locals::StoreOp>(op, ctx).is_some()
                {
                    return true;
                } else if Operation::get_op::<CallOp>(op, ctx).is_some()
                    || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                {
                    let mut arg_aliases = false;
                    for operand in op.deref(ctx).operands() {
                        let (arg_cells, arg_untracked) = root_heap_cells(ctx, state, operand);
                        let (arg_objs, arg_open) =
                            collect_reachable_heap(&self.heap, &arg_cells, arg_untracked);
                        if !reachable_objects.is_disjoint(&arg_objs)
                            || (open_or_summarized && arg_open)
                        {
                            arg_aliases = true;
                            break;
                        }
                    }
                    if arg_aliases {
                        return true;
                    }
                    if open_or_summarized {
                        let callee = Operation::get_op::<CallOp>(op, ctx)
                            .map(|c| c.callee(ctx))
                            .or_else(|| {
                                Operation::get_op::<AsyncCallOp>(op, ctx).map(|c| c.callee(ctx))
                            });
                        if callee.is_none_or(|c| self.callee_may_mutate_open_state(ctx, &c)) {
                            return true;
                        }
                    }
                }
            }

            if (unwrap_may_throw || seen_intermediate_parse)
                && (Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
                    || Operation::get_op::<CallOp>(op, ctx).is_some()
                    || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                    || Operation::get_op::<AwaitOp>(op, ctx).is_some()
                    || Operation::get_op::<TimerOp>(op, ctx).is_some()
                    || Operation::get_op::<crate::exceptions::RaiseOp>(op, ctx).is_some()
                    || Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx).is_some())
            {
                return true;
            }
        }
        false
    }

    fn callee_may_mutate_open_state(&self, ctx: &Context, root: &Identifier) -> bool {
        let mut visited = HashSet::new();
        let mut stack = vec![root.clone()];
        while let Some(name) = stack.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some(body) = self.bodies.get(&name) else {
                return true;
            };
            if body.effects.effects.may_mutate_records() {
                return true;
            }
            for &constraint in &body.constraints {
                let Constraint::Op { op, .. } = constraint else {
                    continue;
                };
                if Operation::get_op::<FieldSetOp>(op, ctx).is_some()
                    || Operation::get_op::<ArraySetOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayExtendOp>(op, ctx).is_some()
                    || Operation::get_op::<ArrayTruncateOp>(op, ctx).is_some()
                    || Operation::get_op::<crate::locals::StoreOp>(op, ctx).is_some()
                    || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
                {
                    return true;
                }
                if let Some(callee) = Operation::get_op::<CallOp>(op, ctx)
                    .map(|c| c.callee(ctx))
                    .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|c| c.callee(ctx)))
                {
                    stack.push(callee);
                }
            }
        }
        false
    }
}

