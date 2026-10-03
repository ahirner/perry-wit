//! Mutable byte views with distinct identity and shared backing allocation ownership.

use std::collections::BTreeMap;

use anyhow::Result;
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use waffle::{Func, Memory, Module};

use super::{allocation::AllocationFuncs, runtime, visit};

#[derive(Clone, Copy)]
pub(crate) struct ByteHelpers {
    pub(crate) lift_canonical: Func,
    pub(crate) new: Func,
    pub(crate) copy: Func,
    pub(crate) get: Func,
    pub(crate) set: Func,
    pub(crate) subarray: Func,
}

pub(crate) fn is_byte_view(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == "Uint8Array")
}

pub(crate) fn required(hir: &HirModule) -> bool {
    let mut required = false;
    for function in &hir.functions {
        required |= contains_byte_view(&function.return_type)
            || function
                .params
                .iter()
                .any(|param| contains_byte_view(&param.ty));
        visit::visit_function_expressions(function, &mut |expression| {
            required |= matches!(expression, perry_hir::ir::Expr::Uint8ArrayNew(_));
        });
    }
    required
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<ByteHelpers> {
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("bytes/runtime.wat"),
        &BTreeMap::from([("realloc", allocator.realloc)]),
    )?;
    Ok(ByteHelpers {
        lift_canonical: functions["bytes.lift"],
        new: functions["bytes.new"],
        copy: functions["bytes.copy"],
        get: functions["bytes.get"],
        set: functions["bytes.set"],
        subarray: functions["bytes.subarray"],
    })
}

fn contains_byte_view(ty: &HirType) -> bool {
    match ty {
        HirType::Promise(inner) => contains_byte_view(inner),
        HirType::Generic { base, type_args } if base == "Result" => {
            type_args.iter().any(contains_byte_view)
        }
        ty => is_byte_view(ty),
    }
}
