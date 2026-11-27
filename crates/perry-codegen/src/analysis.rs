//! Reachable MIR functions and their signatures.
use crate::Exports;
use perry_mir::{PlironResultExt as _, is_exported};
use perry_wit_diagnostics::Result;
use pliron::{
    builtin::{
        op_interfaces::SymbolOpInterface,
        ops::{FuncOp, ModuleOp},
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    context::Context,
    op::Op,
    r#type::{TypeHandle, TypedHandle},
};

/// Collect functions reachable from boundary exports and module initialization, preserving
/// source order for deterministic dispatch IDs.
pub(crate) fn reachable_functions(
    ctx: &Context,
    module: &ModuleOp,
    exports: &Exports,
) -> Vec<FuncOp> {
    perry_mir::reachable_functions(ctx, module.get_operation(), |function| {
        let name = function.get_symbol_name(ctx);
        let exported = match exports {
            Exports::Raw | Exports::RawChecked => is_exported(ctx, function),
            Exports::Canonical { exports, .. } => exports
                .iter()
                .any(|export| export.function == name.as_ref()),
        };
        (exported || name.as_ref() == perry_mir::INITIALIZER).then_some(name)
    })
}

pub(crate) fn function_type(ctx: &Context, function: &FuncOp) -> Result<TypedHandle<FunctionType>> {
    TypedHandle::from_handle(function.get_type(ctx), ctx).to_diag(ctx)
}

pub(crate) fn function_signature(
    ctx: &Context,
    function: &FuncOp,
) -> Result<(Vec<TypeHandle>, Vec<TypeHandle>)> {
    let ty = function_type(ctx, function)?;
    let ty = ty.deref(ctx);
    Ok((ty.arg_types(), ty.res_types()))
}
