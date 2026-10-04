//! Immutable Date values, UTC calendar arithmetic, and ISO formatting.

use super::{allocation::AllocationFuncs, runtime};
use anyhow::Result;
use perry_hir::{
    ir::{Expr, Module as HirModule},
    types::Type as HirType,
};
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) const DATE_TYPE: &str = "__perry_internal_date";

#[derive(Clone, Copy)]
pub(crate) struct DateHelpers {
    pub(crate) new: Func,
    pub(crate) part: Func,
    pub(crate) iso: Func,
}

pub(crate) fn is_date(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == DATE_TYPE)
}

pub(crate) fn required(hir: &HirModule) -> bool {
    hir.functions.iter().any(|function| {
        let mut required = contains_date(&function.return_type)
            || function.params.iter().any(|param| contains_date(&param.ty));
        super::visit::visit_function_expressions(function, &mut |expression| {
            if let Expr::ExternFuncRef { return_type, .. } = expression {
                required |= is_date(return_type);
            }
        });
        required
    })
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
) -> Result<DateHelpers> {
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("date/runtime.wat"),
        &BTreeMap::from([("realloc", allocator.realloc)]),
    )?;
    Ok(DateHelpers {
        new: functions["date.new"],
        part: functions["date.part"],
        iso: functions["date.iso"],
    })
}

fn contains_date(ty: &HirType) -> bool {
    match ty {
        HirType::Promise(inner) | HirType::Array(inner) => contains_date(inner),
        HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(contains_date),
        HirType::Object(fields) => {
            fields
                .properties
                .values()
                .any(|property| contains_date(&property.ty))
                || fields.index_signature.as_deref().is_some_and(contains_date)
        }
        _ => is_date(ty),
    }
}
