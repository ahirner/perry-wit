//! Leases: static liveness for objects a built-in hands out again on its next call
//! (design decision D3, "static liveness first").
//!
//! `reader.read()` returns a promise, a `{ done, value }` result and a `Uint8Array` over the
//! chunk; `writer.write(bytes)` returns a promise. In a streaming loop each call's objects die
//! before the next call, but a bump region would keep all of them until the task ends. An
//! operation is **leased** when the MIR proves that nothing derived from any call of its class
//! is live when one of them runs again; the runtime then reuses one set of objects (and one
//! chunk buffer) per reader or writer, so a stream of any length runs in constant memory.
//!
//! The proof, per function and class:
//!
//! - every receiver (reader or writer) is used only by built-ins, so no other function can
//!   call the class on it;
//! - the values derived from the results (through `await`, `.value`, `subarray`, optional
//!   wrapping and block arguments) are used only by operations that read or copy them, never
//!   stored, returned, passed to a function or captured;
//! - none of them is live after any operation of the class, except that operation's own
//!   result (backward liveness over the MIR CFG).
//!
//! Without the proof the operations allocate fresh objects, which is always correct.
use perry_mir::{
    calls::AwaitOp,
    intrinsics::{Intrinsic, IntrinsicOp},
    optional::{IsUndefinedOp, UnwrapOp, WrapOp},
    scalar::CompareOp,
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    op::op_cast,
    operation::Operation,
    value::Value,
};
use std::collections::{HashMap, HashSet};

/// Classes of leasable operations: each operation's objects are reused by the next call of the
/// same class.
const CLASSES: [&[Intrinsic]; 2] = [&[Intrinsic::ReaderRead], &[Intrinsic::WriterWrite]];

fn intrinsic(ctx: &Context, op: Ptr<Operation>) -> Option<Intrinsic> {
    Operation::get_op::<IntrinsicOp>(op, ctx).and_then(|i| i.intrinsic(ctx))
}

/// The leased operations of `function`.
pub(crate) fn leased(ctx: &Context, function: &FuncOp) -> HashSet<Ptr<Operation>> {
    let ops: Vec<Ptr<Operation>> = function
        .get_region(ctx)
        .deref(ctx)
        .iter(ctx)
        .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
        .collect();
    let mut leased = HashSet::new();
    let mut live_after = None;
    for class in CLASSES {
        let members: Vec<_> = ops
            .iter()
            .copied()
            .filter(|op| intrinsic(ctx, *op).is_some_and(|i| class.contains(&i)))
            .collect();
        if members.is_empty() || !receivers_stay_local(ctx, &members) {
            continue;
        }
        let Some(derived) = derived(ctx, &members) else {
            continue;
        };
        let live = live_after.get_or_insert_with(|| live_after_ops(ctx, function));
        let dead = members.iter().all(|op| {
            let result = op.deref(ctx).get_result(0);
            live.get(op)
                .is_none_or(|values| values.iter().all(|v| *v == result || !derived.contains(v)))
        });
        if dead {
            leased.extend(members);
        }
    }
    leased
}

/// Every receiver is used by built-ins only.
fn receivers_stay_local(ctx: &Context, members: &[Ptr<Operation>]) -> bool {
    members.iter().all(|op| {
        let receiver = op.deref(ctx).get_operand(0);
        receiver
            .uses(ctx)
            .iter()
            .all(|u| intrinsic(ctx, u.user_op()).is_some())
    })
}

/// Values derived from the members' results, or `None` if one escapes.
fn derived(ctx: &Context, members: &[Ptr<Operation>]) -> Option<HashSet<Value>> {
    let mut derived: HashSet<Value> = HashSet::new();
    let mut work: Vec<Value> = members
        .iter()
        .map(|op| op.deref(ctx).get_result(0))
        .collect();
    while let Some(value) = work.pop() {
        if !derived.insert(value) {
            continue;
        }
        for r#use in value.uses(ctx) {
            let user = r#use.user_op();
            let operation = user.deref(ctx);
            let result = || operation.get_result(0);
            if Operation::get_op::<AwaitOp>(user, ctx).is_some()
                || Operation::get_op::<UnwrapOp>(user, ctx).is_some()
                || Operation::get_op::<WrapOp>(user, ctx).is_some()
            {
                work.push(result());
            } else if Operation::get_op::<IsUndefinedOp>(user, ctx).is_some()
                || Operation::get_op::<CompareOp>(user, ctx).is_some()
            {
                // Tests read the value.
            } else if let Some(builtin) = intrinsic(ctx, user) {
                match builtin {
                    // Views of the leased object.
                    Intrinsic::ReadValue | Intrinsic::BytesSubarray => work.push(result()),
                    // Reads and copies.
                    Intrinsic::ReadDone
                    | Intrinsic::BytesLength
                    | Intrinsic::BytesGet
                    | Intrinsic::BytesSetFrom
                    | Intrinsic::BytesCopy
                    | Intrinsic::BytesSlice
                    | Intrinsic::TextDecode
                    | Intrinsic::WriterWrite => {}
                    _ => return None,
                }
            } else {
                // A branch passes the value to a block argument; anything else lets it escape.
                let dynamic = Operation::get_op_dyn(user, ctx);
                let branch = op_cast::<dyn BranchOpInterface>(dynamic.as_ref())?;
                let index = r#use.find_index(ctx);
                let (block, argument): (Ptr<BasicBlock>, usize) =
                    (0..operation.get_num_successors()).find_map(|succ| {
                        let range = branch.successor_operand_range(ctx, succ);
                        range
                            .contains(&index)
                            .then(|| (operation.get_successor(succ), index - range.start))
                    })?;
                work.push(block.deref(ctx).get_argument(argument));
            }
        }
    }
    Some(derived)
}

/// The values live right after each operation (backward liveness over the CFG).
fn live_after_ops(ctx: &Context, function: &FuncOp) -> HashMap<Ptr<Operation>, HashSet<Value>> {
    let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
    let mut live_in: HashMap<Ptr<BasicBlock>, HashSet<Value>> = HashMap::new();
    let live_out = |live_in: &HashMap<Ptr<BasicBlock>, HashSet<Value>>, block: Ptr<BasicBlock>| {
        let mut live = HashSet::new();
        for succ in block.deref(ctx).succs(ctx) {
            live.extend(live_in.get(&succ).into_iter().flatten().copied());
        }
        live
    };
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut live = live_out(&live_in, block);
            let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
            for op in ops.iter().rev() {
                for result in op.deref(ctx).results() {
                    live.remove(&result);
                }
                live.extend(op.deref(ctx).operands());
            }
            for argument in block.deref(ctx).arguments() {
                live.remove(&argument);
            }
            if live_in.get(&block) != Some(&live) {
                live_in.insert(block, live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut after = HashMap::new();
    for &block in &blocks {
        let mut live = live_out(&live_in, block);
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        for op in ops.iter().rev() {
            after.insert(*op, live.clone());
            for result in op.deref(ctx).results() {
                live.remove(&result);
            }
            live.extend(op.deref(ctx).operands());
        }
    }
    after
}
