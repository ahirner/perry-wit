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

struct Transfer {
    block: Ptr<BasicBlock>,
    argument: Value,
}

/// In-place string appends: `a + b` becomes `perry.string_append` when no one else holds
/// `a`'s header and `a` is not live afterwards, so the runtime updates that header instead of
/// allocating one per concatenation.
///
/// A string value is **owned** when only it refers to its header: it is the result of a
/// concatenation or an immutable view (both return fresh buffered headers), or a block
/// argument all of whose incoming values are owned or constants (an optimistic fixpoint, so
/// the loop variable of `text = text + x` qualifies), and each of its uses either only reads
/// it, consumes its final use, or transfers it to an owned block argument after its final
/// direct observation on that edge. A consuming call or store may retain the header, but no
/// subsequent append can reuse that value. Constants and character lookups may produce
/// shared bufferless headers; the runtime preserves those headers and allocates when their
/// text must grow.
pub(crate) fn append_in_place(root: Ptr<Operation>, ctx: &mut Context) -> IRStatus {
    use crate::{
        collections::{StringAppendOp, StringConcatOp, StringConstOp, StringLenOp},
        control::ReturnOp,
        intrinsics::{Intrinsic, IntrinsicOp},
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
        let mut passed: HashMap<(Ptr<Operation>, Value), Vec<Transfer>> = HashMap::new();
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
                        .push(Transfer {
                            block: dest,
                            argument,
                        });
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
        let fresh_header = |ctx: &Context, op: Ptr<Operation>| {
            is_concat(ctx, op)
                || Operation::get_op::<IntrinsicOp>(op, ctx).is_some_and(|op| {
                    matches!(
                        op.intrinsic(ctx),
                        Some(
                            Intrinsic::StringCharAt
                                | Intrinsic::StringCharAtIndex
                                | Intrinsic::StringSlice
                                | Intrinsic::StringSubstring
                        )
                    )
                })
        };
        let reads = |ctx: &Context, op: Ptr<Operation>| {
            is_concat(ctx, op)
                || Operation::get_op::<StringLenOp>(op, ctx).is_some()
                || Operation::get_op::<CompareOp>(op, ctx).is_some()
                || Operation::get_op::<ReturnOp>(op, ctx).is_some()
                || Operation::get_op::<IntrinsicOp>(op, ctx).is_some_and(|op| {
                    matches!(
                        op.intrinsic(ctx),
                        Some(
                            Intrinsic::StringCharAt
                                | Intrinsic::StringCharAtIndex
                                | Intrinsic::StringIndex
                                | Intrinsic::StringCodePointAt
                                | Intrinsic::StringSlice
                                | Intrinsic::StringSubstring
                                | Intrinsic::StringIndexOf
                                | Intrinsic::StringLastIndexOf
                                | Intrinsic::StringStartsWith
                                | Intrinsic::StringEndsWith
                        )
                    )
                })
        };
        let mut owned: HashSet<Value> = HashSet::new();
        for &block in &blocks {
            for argument in block.deref(ctx).arguments() {
                if incoming.contains_key(&argument) && is_string(ctx, argument) {
                    owned.insert(argument);
                }
            }
            for op in block.deref(ctx).iter(ctx) {
                if fresh_header(ctx, op) {
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
                        // Transfer the header only after its final direct observation.
                        let Some(arguments) = passed.get(&(user, value)) else {
                            return !live_after(ctx, &successors, value, user);
                        };
                        arguments.len() == 1
                            && owned.contains(&arguments[0].argument)
                            && !live_in_blocks(ctx, &successors, value, vec![arguments[0].block])
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
    live_in_blocks(
        ctx,
        successors,
        value,
        successors.get(&block).cloned().unwrap_or_default(),
    )
}

/// Direct observations on paths entered through these blocks, before the next definition.
/// A branch transfer checks only its receiving edge; other edges remain independent.
fn live_in_blocks(
    ctx: &Context,
    successors: &HashMap<Ptr<BasicBlock>, Vec<Ptr<BasicBlock>>>,
    value: Value,
    mut work: Vec<Ptr<BasicBlock>>,
) -> bool {
    let Some(defined) = value.get_defining_block(ctx) else {
        return true;
    };
    let user_blocks: HashSet<Ptr<BasicBlock>> = value
        .uses(ctx)
        .iter()
        .filter_map(|usage| usage.user_op().deref(ctx).get_parent_block())
        .collect();
    let mut seen = HashSet::new();
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
