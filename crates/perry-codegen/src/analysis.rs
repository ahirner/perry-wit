//! Whole-module analyses over MIR functions: reachability and a
//! deterministic value order.
use crate::Exports;
use anyhow::{Context as _, Result, anyhow};
use perry_mir::{AsyncCallOp, CallOp, LiveValues, ProgramFacts, is_exported};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SingleBlockRegionInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
    },
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    pass::AnalysisManager,
    printable::Printable,
    value::Value,
};
use std::{
    cell::Ref,
    collections::{HashMap, HashSet},
};

/// Borrow both analyses after computing them, so computing one cannot invalidate the borrow of the other.
pub(super) struct ModuleAnalyses<'a> {
    pub(super) facts: Ref<'a, ProgramFacts>,
    pub(super) live: Ref<'a, LiveValues>,
}

pub(super) fn module_analyses<'a>(
    ctx: &Context,
    module: &ModuleOp,
    analyses: &'a mut AnalysisManager,
) -> Result<ModuleAnalyses<'a>> {
    let op = module.get_operation();
    analyses
        .get_analysis::<ProgramFacts>(op, ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    analyses
        .get_analysis::<LiveValues>(op, ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    Ok(ModuleAnalyses {
        facts: analyses
            .try_get_analysis::<ProgramFacts>(op)
            .context("computed program facts are missing")?,
        live: analyses
            .try_get_analysis::<LiveValues>(op)
            .context("computed liveness is missing")?,
    })
}

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
        if exported || name.as_ref() == perry_mir::INITIALIZER {
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
