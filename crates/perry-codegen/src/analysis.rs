//! Whole-module analyses over MIR functions: reachability, exception classification and a
//! deterministic value order.
use crate::Exports;
use perry_mir::{
    calls::{AsyncCallOp, CallOp, is_exported},
    intrinsics::may_throw,
};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SingleBlockRegionInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    operation::Operation,
    value::Value,
};
use std::collections::{HashMap, HashSet};

pub(crate) fn module_functions(ctx: &Context, module: &ModuleOp) -> Vec<FuncOp> {
    module
        .get_body(ctx, 0)
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .collect()
}

pub(crate) fn function_ops(ctx: &Context, function: &FuncOp) -> Vec<Ptr<Operation>> {
    let mut ops = Vec::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        ops.extend(block.deref(ctx).iter(ctx));
    }
    ops
}

/// MIR calls name their targets directly; only boundary exports and module initialization
/// can enter from outside this graph. Preserve source order for deterministic dispatch IDs.
pub(crate) fn reachable_functions(
    ctx: &Context,
    module: &ModuleOp,
    exports: &Exports,
) -> Vec<FuncOp> {
    let functions = module_functions(ctx, module);
    let indices: HashMap<_, _> = functions
        .iter()
        .enumerate()
        .map(|(index, function)| (function.get_symbol_name(ctx), index))
        .collect();
    let mut pending = Vec::new();
    let mut reachable = HashSet::new();
    for (index, function) in functions.iter().enumerate() {
        let name = function.get_symbol_name(ctx);
        let exported = match exports {
            Exports::Raw | Exports::RawChecked => is_exported(ctx, function),
            Exports::Canonical { exports, .. } => exports
                .iter()
                .any(|export| export.function == name.as_ref()),
        };
        if exported || name.as_ref() == perry_mir::globals::INITIALIZER {
            reachable.insert(index);
            pending.push(index);
        }
    }
    while let Some(index) = pending.pop() {
        for op in function_ops(ctx, &functions[index]) {
            let callee = Operation::get_op::<CallOp>(op, ctx)
                .map(|call| call.callee(ctx))
                .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|call| call.callee(ctx)));
            if let Some(callee) = callee
                && let Some(&index) = indices.get(&callee)
                && reachable.insert(index)
            {
                pending.push(index);
            }
        }
    }
    functions
        .into_iter()
        .enumerate()
        .filter_map(|(index, function)| reachable.contains(&index).then_some(function))
        .collect()
}

/// Definition order, independent of arena addresses and allocation history.
pub(crate) fn value_order(ctx: &Context, function: &FuncOp) -> HashMap<Value, usize> {
    let mut order = HashMap::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        for value in block.deref(ctx).arguments() {
            order.insert(value, order.len());
        }
        for op in block.deref(ctx).iter(ctx) {
            for value in op.deref(ctx).results() {
                order.insert(value, order.len());
            }
        }
    }
    order
}

/// Functions that may complete with an exception: those with a throwing op other than a call,
/// and, to a fixpoint, those calling one of them.
pub(crate) fn throwing_functions(ctx: &Context, functions: &[FuncOp]) -> HashSet<Identifier> {
    let mut throws = HashSet::new();
    let mut calls: Vec<(Identifier, Vec<Identifier>)> = Vec::new();
    for function in functions {
        let name = function.get_symbol_name(ctx);
        // An async function rejects its promise instead of returning a status.
        if perry_mir::calls::is_async(ctx, function) {
            continue;
        }
        let mut callees = Vec::new();
        for op in function_ops(ctx, function) {
            if let Some(call) = Operation::get_op::<CallOp>(op, ctx) {
                callees.push(call.callee(ctx));
            } else if may_throw(ctx, op) {
                throws.insert(name.clone());
            }
        }
        calls.push((name, callees));
    }
    loop {
        let before = throws.len();
        for (name, callees) in &calls {
            if callees.iter().any(|callee| throws.contains(callee)) {
                throws.insert(name.clone());
            }
        }
        if throws.len() == before {
            return throws;
        }
    }
}
