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
    pub(crate) take_strings: Func,
    pub(crate) lower_strings: Func,
}

pub(crate) fn is_string_array(ty: &HirType) -> bool {
    matches!(ty,HirType::Array(inner) if **inner == HirType::String)
}

pub(crate) fn required(hir: &HirModule) -> bool {
    let needed =
        |ty: &HirType| super::visit::contains_type(ty, |ty| is_stats(ty) || is_string_array(ty));
    hir.extern_funcs
        .iter()
        .any(|(_, params, result)| params.iter().chain(std::iter::once(result)).any(needed))
        || hir.functions.iter().any(|function| {
            let mut required = std::iter::once(&function.return_type)
                .chain(function.params.iter().map(|param| &param.ty))
                .any(needed);
            super::visit::visit_statement_nodes(&function.body, &mut |statement| {
                if let perry_hir::ir::Stmt::Let { ty, .. } = statement {
                    required |= needed(ty);
                }
            });
            required
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
        take_strings: functions["strings.take"],
        lower_strings: functions["strings.lower"],
    })
}
