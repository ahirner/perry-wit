//! Reuse uniquely owned string headers after their final observation.
use crate::aggregates::{functions, operations};
use pliron::{
    basic_block::BasicBlock,
    context::{Context, Ptr},
    irbuild::IRStatus,
    linked_list::{ContainsLinkedList, LinkedList},
    op::Op,
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use std::collections::{HashMap, HashSet};

/// In-place string appends: `a + b` becomes `perry.string_append` when no one else holds
/// `a`'s header and `a` is not live afterwards, so the runtime updates that header instead of
/// allocating one per concatenation.
///
/// A string value is **owned** when only it refers to its header: it is the result of a
/// concatenation (`perry_string_concat` never shares a header with a buffer) or a block
/// argument all of whose incoming values are owned or constants (an optimistic fixpoint, so
/// the loop variable of `text = text + x` qualifies), and each of its uses either only reads
/// it (length, comparison, an operand of a concatenation) or passes it, as its only use, to
/// an owned block argument. Only constants may enter this ownership set without a fresh header. The runtime
/// preserves bufferless headers and allocates a new one when their text must grow.
pub fn append_in_place(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    use crate::{
        collections::{StringAppendOp, StringConcatOp, StringConstOp, StringLenOp},
        scalar::CompareOp,
        types::StringType,
    };
    use pliron::builtin::op_interfaces::{BranchOpInterface, OneRegionInterface};
    let mut status = IRStatus::Unchanged;
    for function in functions(root, ctx) {
        let blocks: Vec<Ptr<BasicBlock>> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
        // Block arguments with their incoming values, and branches with the arguments each
        // of their operands is passed to.
        let mut incoming: HashMap<Value, Vec<Value>> = HashMap::new();
        let mut passed: HashMap<(Ptr<Operation>, Value), Vec<Value>> = HashMap::new();
        let mut successors: HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>> = HashMap::new();
        for &block in &blocks {
            let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
                continue;
            };
            let dests: Vec<Ptr<BasicBlock>> = terminator.deref(ctx).successors().collect();
            successors.insert(block, dests.clone());
            let dyn_op = Operation::get_op_dyn(terminator, ctx);
            let Some(branch) = pliron::op::op_cast::<dyn BranchOpInterface>(dyn_op.as_ref()) else {
                continue;
            };
            for (succ, dest) in dests.into_iter().enumerate() {
                for (index, value) in branch.successor_operands(ctx, succ).into_iter().enumerate() {
                    let argument = dest.deref(ctx).get_argument(index);
                    incoming.entry(argument).or_default().push(value);
                    passed
                        .entry((terminator, value))
                        .or_default()
                        .push(argument);
                }
            }
        }
        let is_string = |ctx: &Context, v: Value| v.get_type(ctx).deref(ctx).is::<StringType>();
        let is_concat = |ctx: &Context, op: Ptr<Operation>| {
            Operation::get_op::<StringConcatOp>(op, ctx).is_some()
                || Operation::get_op::<StringAppendOp>(op, ctx).is_some()
        };
        let is_constant = |ctx: &Context, v: Value| {
            v.defining_op()
                .is_some_and(|op| Operation::get_op::<StringConstOp>(op, ctx).is_some())
        };
        let reads = |ctx: &Context, op: Ptr<Operation>| {
            is_concat(ctx, op)
                || Operation::get_op::<StringLenOp>(op, ctx).is_some()
                || Operation::get_op::<CompareOp>(op, ctx).is_some()
        };
        let mut owned: HashSet<Value> = HashSet::new();
        for &block in &blocks {
            for argument in block.deref(ctx).arguments() {
                if incoming.contains_key(&argument) && is_string(ctx, argument) {
                    owned.insert(argument);
                }
            }
            for op in block.deref(ctx).iter(ctx) {
                if is_concat(ctx, op) {
                    owned.insert(op.deref(ctx).get_result(0));
                }
            }
        }
        loop {
            let removed: Vec<Value> = owned
                .iter()
                .copied()
                .filter(|&value| {
                    if value.defining_op().is_none()
                        && !incoming[&value]
                            .iter()
                            .all(|v| owned.contains(v) || is_constant(ctx, *v))
                    {
                        return true;
                    }
                    let uses = value.uses(ctx);
                    !uses.iter().all(|use_| {
                        let user = use_.user_op();
                        if reads(ctx, user) {
                            return true;
                        }
                        // Passed on: its only use, to one owned argument.
                        uses.len() == 1
                            && passed.get(&(user, value)).is_some_and(|arguments| {
                                arguments.len() == 1 && owned.contains(&arguments[0])
                            })
                    })
                })
                .collect();
            if removed.is_empty() {
                break;
            }
            for value in removed {
                owned.remove(&value);
            }
        }
        for op in operations(ctx, &function) {
            if Operation::get_op::<StringConcatOp>(op, ctx).is_none() {
                continue;
            }
            let (a, b) = {
                let operation = op.deref(ctx);
                (operation.get_operand(0), operation.get_operand(1))
            };
            if a == b || !owned.contains(&a) || live_after(ctx, &successors, a, op) {
                continue;
            }
            let append = StringAppendOp::new(ctx, a, b).get_operation();
            append.insert_before(ctx, op);
            let (old, new) = (op.deref(ctx).get_result(0), append.deref(ctx).get_result(0));
            old.replace_all_uses_with(ctx, &new);
            Operation::erase(op, ctx);
            status = IRStatus::Changed;
        }
    }
    status
}

/// Whether `value` may be used after `op`: later in its block, or in a block reachable from
/// there before `value` is defined again.
fn live_after(
    ctx: &Context,
    successors: &HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    value: Value,
    op: Ptr<Operation>,
) -> bool {
    let block = op.deref(ctx).get_parent_block().expect("in a block");
    let users: HashSet<Ptr<Operation>> = value.uses(ctx).iter().map(|u| u.user_op()).collect();
    let mut next = op.deref(ctx).get_next();
    while let Some(later) = next {
        if users.contains(&later) {
            return true;
        }
        next = later.deref(ctx).get_next();
    }
    let Some(defined) = value.get_defining_block(ctx) else {
        return true;
    };
    let user_blocks: HashSet<Ptr<BasicBlock>> = users
        .iter()
        .filter_map(|user| user.deref(ctx).get_parent_block())
        .collect();
    let mut seen = HashSet::new();
    let mut work: Vec<Ptr<BasicBlock>> = successors.get(&block).cloned().unwrap_or_default();
    while let Some(next) = work.pop() {
        // Reaching the definition again starts a new value.
        if next == defined || !seen.insert(next) {
            continue;
        }
        if user_blocks.contains(&next) {
            return true;
        }
        work.extend(successors.get(&next).into_iter().flatten().copied());
    }
    false
}
