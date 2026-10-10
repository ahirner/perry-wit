//! Bounded leaf inlining and scalar replacement of closed records.
use crate::{
    calls::{AsyncCallOp, AwaitOp, CallOp, is_async},
    control::ReturnOp,
    exceptions::{CaughtOp, MayThrowInterface},
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    locals::{LoadOp, LocalOp, StoreOp},
    types::RecordType,
};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, OneResultInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::walkers::{IRNode, WALKCONFIG_PREORDER_FORWARD, uninterruptible::immutable::walk_op},
    irbuild::{
        cloning::{IrMapping, clone_operation},
        inserter::Inserter,
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::{ContainsLinkedList, LinkedList},
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::{Typed, TypedHandle},
};
use std::collections::HashMap;

pub(crate) fn functions(root: Ptr<Operation>, ctx: &Context) -> Vec<FuncOp> {
    let mut functions = Vec::new();
    walk_op(
        ctx,
        &mut functions,
        &WALKCONFIG_PREORDER_FORWARD,
        root,
        |ctx, functions, node| {
            if let IRNode::Operation(op) = node
                && let Some(function) = Operation::get_op::<FuncOp>(op, ctx)
            {
                functions.push(function);
            }
        },
    );
    functions
}

/// Inline small synchronous leaves, with a fixed growth budget per caller. Throwing leaves
/// and calls with a catch edge stay calls, preserving exception transfer exactly.
pub(crate) fn inline_leaves(root: Ptr<Operation>, ctx: &mut Context) {
    const MAX_LEAF_OPS: usize = 24;
    const MAX_GROWTH: usize = 256;
    let functions = functions(root, ctx);
    let mut leaves = HashMap::new();
    for function in &functions {
        let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
        if is_async(ctx, function) || blocks.len() != 1 {
            continue;
        }
        let block = blocks[0];
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let Some((&ret, body)) = ops.split_last() else {
            continue;
        };
        if body.len() > MAX_LEAF_OPS || Operation::get_op::<ReturnOp>(ret, ctx).is_none() {
            continue;
        }
        if body.iter().any(|&op| {
            op.deref(ctx).num_regions() != 0
                || op.deref(ctx).get_num_successors() != 0
                || Operation::get_op::<CallOp>(op, ctx).is_some()
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                || Operation::get_op::<AwaitOp>(op, ctx).is_some()
                || op_cast::<dyn MayThrowInterface>(Operation::get_op_dyn(op, ctx).as_ref())
                    .is_some()
        }) {
            continue;
        }
        leaves.insert(function.get_symbol_name(ctx), (block, body.to_vec(), ret));
    }
    let mut rewriter = IRRewriter::<DummyListener>::default();
    for function in functions {
        let mut budget = MAX_GROWTH;
        let calls: Vec<_> = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .filter_map(|op| Operation::get_op::<CallOp>(op, ctx))
            .collect();
        for call in calls {
            let Some((entry, body, ret)) = leaves.get(&call.callee(ctx)) else {
                continue;
            };
            let op = call.get_operation();
            if body.len() > budget
                || op
                    .deref(ctx)
                    .get_next()
                    .is_some_and(|next| Operation::get_op::<CaughtOp>(next, ctx).is_some())
            {
                continue;
            }
            budget -= body.len();
            let mut mapping = IrMapping::new();
            for (formal, actual) in entry.deref(ctx).arguments().zip(op.deref(ctx).operands()) {
                mapping.map_value(formal, actual);
            }
            rewriter.set_insertion_point_before_operation(op);
            for &original in body {
                let copy = clone_operation(original, ctx, &mut rewriter, &mut mapping);
                rewriter.insert_operation(ctx, copy);
            }
            let results: Vec<_> = ret
                .deref(ctx)
                .operands()
                .map(|v| mapping.lookup_value_or_default(v))
                .collect();
            rewriter.replace_operation_with_values(ctx, op, results);
        }
    }
}

/// Split a record into local field slots only when all uses are direct field reads/writes.
/// Entry slots plus stores at the original allocation preserve per-iteration initialization;
/// mem2reg then removes the slots and supplies the required control-flow joins.
pub(crate) fn scalar_replace(root: Ptr<Operation>, ctx: &mut Context) -> Result<bool> {
    let mut changed = false;
    let mut rewriter = IRRewriter::<DummyListener>::default();
    for function in functions(root, ctx) {
        let allocations: Vec<_> = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .filter_map(|op| Operation::get_op::<AllocOp>(op, ctx))
            .collect();
        for allocation in allocations {
            let reference = allocation.get_result(ctx);
            let uses = reference.uses(ctx);
            if uses.iter().any(|u| {
                u.find_index(ctx) != 0
                    || (Operation::get_op::<FieldGetOp>(u.user_op(), ctx).is_none()
                        && Operation::get_op::<FieldSetOp>(u.user_op(), ctx).is_none())
            }) {
                continue;
            }
            let record = TypedHandle::<RecordType>::from_handle(reference.get_type(ctx), ctx)?;
            let fields = record.deref(ctx).fields()?.to_vec();
            let initial: Vec<_> = allocation.get_operation().deref(ctx).operands().collect();
            let mut slots = HashMap::new();
            rewriter.set_insertion_point_to_block_start(function.get_entry_block(ctx));
            for (name, ty) in fields {
                let slot = LocalOp::new(ctx, ty);
                rewriter.insert_op(ctx, &slot);
                slots.insert(name, slot.get_result(ctx));
            }
            rewriter.set_insertion_point_before_operation(allocation.get_operation());
            let names: Vec<_> = record
                .deref(ctx)
                .fields()?
                .iter()
                .map(|(name, _)| name.clone())
                .collect();
            for (name, value) in names.into_iter().zip(initial) {
                let store = StoreOp::new(ctx, slots[&name], value);
                rewriter.insert_op(ctx, &store);
            }
            for use_ in uses {
                let op = use_.user_op();
                rewriter.set_insertion_point_before_operation(op);
                if let Some(get) = Operation::get_op::<FieldGetOp>(op, ctx) {
                    let load = LoadOp::new(ctx, slots[&get.field(ctx)])?;
                    rewriter.insert_op(ctx, &load);
                    rewriter.replace_operation(ctx, op, load.get_operation());
                } else if let Some(set) = Operation::get_op::<FieldSetOp>(op, ctx) {
                    let value = op.deref(ctx).get_operand(1);
                    let store = StoreOp::new(ctx, slots[&set.field(ctx)], value);
                    rewriter.insert_op(ctx, &store);
                    rewriter.replace_operation(ctx, op, store.get_operation());
                }
            }
            rewriter.erase_operation(ctx, allocation.get_operation());
            changed = true;
        }
    }
    Ok(changed)
}
