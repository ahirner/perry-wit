//! Whole-module analyses over MIR functions: reachability and a
//! deterministic value order.
use crate::Exports;
use anyhow::{Context as _, Result, anyhow};
use perry_mir::{AsyncCallOp, CallOp, LiveValues, ProgramFacts, is_exported};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SymbolOpInterface},
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

/// Precompute and borrow reachability and value-order analyses together.
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
    perry_mir::functions(module.get_operation(), ctx)
}

pub(crate) fn function_ops(ctx: &Context, function: &FuncOp) -> Vec<Ptr<Operation>> {
    perry_mir::operations(ctx, function)
}

/// Collect functions reachable from boundary exports and module initialization, preserving
/// source order for deterministic dispatch IDs.
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

pub(crate) fn function_values(ctx: &Context, function: &FuncOp) -> Vec<Value> {
    let mut values = Vec::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        let block = block.deref(ctx);
        values.extend(block.arguments());
        for op in block.iter(ctx) {
            values.extend(op.deref(ctx).results());
        }
    }
    values
}

pub(crate) fn function_signature(
    ctx: &Context,
    function: &FuncOp,
) -> Result<(
    Vec<pliron::r#type::TypeHandle>,
    Vec<pliron::r#type::TypeHandle>,
)> {
    use pliron::builtin::type_interfaces::FunctionTypeInterface;
    let ty = pliron::r#type::TypedHandle::<pliron::builtin::types::FunctionType>::from_handle(
        pliron::r#type::Typed::get_type(function, ctx),
        ctx,
    )
    .map_err(|_| anyhow::anyhow!("function type"))?;
    let ty = ty.deref(ctx);
    Ok((ty.arg_types(), ty.res_types()))
}

/// Definition order, independent of arena addresses and allocation history.
pub(crate) fn value_order(ctx: &Context, function: &FuncOp) -> HashMap<Value, usize> {
    function_values(ctx, function)
        .into_iter()
        .enumerate()
        .map(|(index, value)| (value, index))
        .collect()
}
