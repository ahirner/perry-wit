//! Tagged guest values shared by object fields and heterogeneous call boundaries.

use super::{allocation::AllocationFuncs, runtime, visit};
use anyhow::{Result, bail};
use perry_hir::{
    ir::{Expr, Module as HirModule, Stmt},
    types::Type as HirType,
};
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) const VALUE_TYPE: &str = "__perry_internal_value";

pub(crate) fn value_type() -> HirType {
    HirType::Named(VALUE_TYPE.into())
}

pub(crate) fn is_dynamic(ty: &HirType) -> bool {
    matches!(ty, HirType::Named(name) if name == VALUE_TYPE)
}

/// Intrinsic `any` operands are validated by each source operation. Guest calls
/// need a concrete heterogeneous representation, distinct from that placeholder.
pub(crate) fn resolve_types(hir: &mut HirModule) {
    for function in &mut hir.functions {
        normalize_type(&mut function.return_type);
        for parameter in &mut function.params {
            normalize_type(&mut parameter.ty);
        }
        visit::visit_statement_nodes_mut(&mut function.body, &mut |statement| {
            if let Stmt::Let { ty, .. } = statement
                && *ty != HirType::Any
            {
                normalize_type(ty);
            }
        });
    }
}

/// Reference tags start at String, as required by the precise heap tracer.
#[derive(Clone, Copy)]
pub(crate) enum ValueTag {
    Undefined = 0,
    Null = 1,
    Boolean = 2,
    Number = 3,
    String = 4,
    Bytes = 5,
    Object = 6,
    Stats = 7,
    StringArray = 8,
    Decoder = 9,
    Promise = 10,
    Date = 11,
}

impl ValueTag {
    pub(crate) fn of(ty: &HirType) -> Result<Self> {
        Ok(match ty {
            HirType::Void => Self::Undefined,
            HirType::Null => Self::Null,
            HirType::Boolean => Self::Boolean,
            HirType::Number => Self::Number,
            HirType::String => Self::String,
            ty if super::bytes::is_byte_view(ty) => Self::Bytes,
            ty if super::objects::is_object(ty) => Self::Object,
            ty if super::filesystem::is_stats(ty) => Self::Stats,
            ty if super::structured::is_string_array(ty) => Self::StringArray,
            ty if super::decoder::is_decoder(ty) => Self::Decoder,
            HirType::Promise(_) => Self::Promise,
            ty if super::date::is_date(ty) => Self::Date,
            _ => bail!("Unsupported tagged value type: {ty:?}"),
        })
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ValueHelpers {
    pub(crate) new: Func,
    pub(crate) extract: Func,
    pub(crate) truthy: Func,
    pub(crate) equal: Func,
    pub(crate) date_number: Func,
    pub(crate) scalar_number: Func,
    pub(crate) async_result: Func,
}

pub(crate) fn required(hir: &HirModule) -> bool {
    hir.functions.iter().any(|function| {
        let mut required = contains_dynamic(&function.return_type)
            || function
                .params
                .iter()
                .any(|param| contains_dynamic(&param.ty));
        visit::visit_function_expressions(function, &mut |expression| {
            required |= matches!(expression, Expr::Object(_) | Expr::New { .. });
        });
        required
    })
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    compare: Func,
) -> Result<ValueHelpers> {
    let functions = runtime::emit_functions(
        module,
        memory,
        include_str!("values/runtime.wat"),
        &BTreeMap::from([("realloc", allocator.realloc), ("compare", compare)]),
    )?;
    Ok(ValueHelpers {
        new: functions["value.new"],
        extract: functions["value.extract"],
        truthy: functions["value.truthy"],
        equal: functions["value.equal"],
        date_number: functions["value.date-number"],
        scalar_number: functions["value.scalar-number"],
        async_result: functions["value.async-result"],
    })
}

fn contains_dynamic(ty: &HirType) -> bool {
    match ty {
        ty if super::objects::is_object(ty) => true,
        HirType::Promise(inner) | HirType::Array(inner) => contains_dynamic(inner),
        HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(contains_dynamic),
        _ => is_dynamic(ty),
    }
}

fn normalize_type(ty: &mut HirType) {
    match ty {
        HirType::Any => *ty = value_type(),
        HirType::Promise(inner) | HirType::Array(inner) => normalize_type(inner),
        HirType::Generic { base, type_args } if base == "Result" => {
            if let Some(success) = type_args.first_mut() {
                normalize_type(success);
            }
        }
        HirType::Generic { type_args, .. }
        | HirType::Union(type_args)
        | HirType::Tuple(type_args) => {
            for ty in type_args {
                normalize_type(ty);
            }
        }
        HirType::Object(object) => {
            for property in object.properties.values_mut() {
                normalize_type(&mut property.ty);
            }
            if let Some(index) = &mut object.index_signature {
                normalize_type(index);
            }
        }
        _ => {}
    }
}
