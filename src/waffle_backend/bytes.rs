//! Mutable byte views with distinct identity and shared backing allocation ownership.

mod buffer;
mod runtime;

use anyhow::Result;
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, visit};

#[derive(Clone, Copy)]
pub(crate) struct ByteHelpers {
    pub(crate) lift_canonical: Func,
    pub(crate) from_buffer: Func,
    pub(crate) validate: Func,
    pub(crate) transfer: Func,
    pub(crate) new: Func,
    pub(crate) copy: Func,
    pub(crate) copy_into: Func,
    pub(crate) get: Func,
    pub(crate) set: Func,
    pub(crate) to_byte: Func,
    pub(crate) subarray: Func,
}

pub(crate) fn is_byte_view(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == "Uint8Array")
}

pub(crate) fn is_array_buffer(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == "ArrayBuffer")
}

pub(crate) fn is_byte_storage(ty: &HirType) -> bool {
    is_byte_view(ty) || is_array_buffer(ty)
}

pub(crate) fn required(hir: &HirModule) -> bool {
    let mut required = hir.extern_funcs.iter().any(|(_, params, result)| {
        params
            .iter()
            .chain(std::iter::once(result))
            .any(contains_byte_view)
    });
    for function in &hir.functions {
        required |= contains_byte_view(&function.return_type)
            || function
                .params
                .iter()
                .any(|param| contains_byte_view(&param.ty));
        visit::visit_function_expressions(function, &mut |expression| {
            required |= matches!(
                expression,
                perry_hir::ir::Expr::Uint8ArrayNew(_) | perry_hir::ir::Expr::TextEncoderEncode(_)
            );
            required |= matches!(expression, perry_hir::ir::Expr::ExternFuncRef { return_type, .. } if contains_byte_view(return_type));
        });
    }
    required
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<ByteHelpers> {
    runtime::emit(module, memory, allocator)
}

fn contains_byte_view(ty: &HirType) -> bool {
    visit::contains_type(ty, is_byte_storage)
}
