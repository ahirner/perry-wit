//! Canonical adapters for retained metadata records and string arrays.

use super::{allocation::AllocationFuncs, filesystem::is_stats, runtime};
use anyhow::Result;
use perry_hir::{ir::Module as HirModule, types::Type as HirType};
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

#[derive(Clone, Copy)]
pub(crate) struct StructuredHelpers {
    pub(crate) lift_stats: Func,
    pub(crate) lift_strings: Func,
    pub(crate) lower_strings: Func,
}

pub(crate) fn is_string_array(ty: &HirType) -> bool {
    matches!(ty,HirType::Array(inner) if **inner == HirType::String)
}

pub(crate) fn contains_stats(ty: &HirType) -> bool {
    contains(ty, is_stats)
}

fn contains(ty: &HirType, predicate: fn(&HirType) -> bool) -> bool {
    if predicate(ty) {
        return true;
    }
    match ty {
        HirType::Promise(inner) | HirType::Array(inner) => contains(inner, predicate),
        HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(|ty| contains(ty, predicate)),
        _ => false,
    }
}

pub(crate) fn required(hir: &HirModule) -> bool {
    hir.functions.iter().any(|function| {
        std::iter::once(&function.return_type)
            .chain(function.params.iter().map(|param| &param.ty))
            .any(|ty| contains(ty, |ty| is_stats(ty) || is_string_array(ty)))
    })
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<StructuredHelpers> {
    let wat = format!(
        "(module (import \"host\" \"realloc\" (func $realloc (param i32 i32 i32 i32) (result i32))) (memory 1) {} {})",
        include_str!("strings/utf8.wat"),
        include_str!("structured/runtime.wat")
    );
    let functions = runtime::emit_functions(
        module,
        memory,
        &wat,
        &BTreeMap::from([("realloc", allocator.realloc)]),
    )?;
    Ok(StructuredHelpers {
        lift_stats: functions["stats.lift"],
        lift_strings: functions["strings.lift"],
        lower_strings: functions["strings.lower"],
    })
}
