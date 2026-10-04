//! Tagged guest values shared by object fields and heterogeneous call boundaries.

use super::{allocation::AllocationFuncs, runtime, visit};
use anyhow::{Result, bail};
use perry_hir::{
    ir::{Expr, Module as HirModule, Stmt},
    types::Type as HirType,
};
use std::collections::BTreeMap;
use waffle::{Func, Memory, Module};

pub(crate) const ARRAY_TYPE: &str = "__perry_internal_array";

pub(crate) const VALUE_TYPE: &str = "__perry_internal_value";

pub(crate) fn is_string_type(ty: &HirType) -> bool {
    matches!(ty, HirType::String | HirType::StringLiteral(_))
        || matches!(ty, HirType::Union(types) if !types.is_empty() && types.iter().all(|ty| matches!(ty, HirType::StringLiteral(_))))
}

pub(crate) fn value_type() -> HirType {
    HirType::Named(VALUE_TYPE.into())
}

pub(crate) fn has_dynamic_properties(ty: &HirType) -> bool {
    is_dynamic(ty) || matches!(ty, HirType::Named(name) if name == ARRAY_TYPE)
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
    Array = 12,
    Instant = 13,
    PlainDateTime = 14,
    HttpResponse = 15,
    WitU64 = 16,
}

impl ValueTag {
    pub(crate) fn of(ty: &HirType) -> Result<Self> {
        Ok(match ty {
            HirType::Void => Self::Undefined,
            HirType::Null => Self::Null,
            HirType::Boolean => Self::Boolean,
            HirType::Number => Self::Number,
            HirType::BigInt => Self::WitU64,
            ty if is_string_type(ty) => Self::String,
            HirType::Tuple(_) => Self::Array,
            HirType::Named(name) if name == ARRAY_TYPE => Self::Array,
            ty if super::bytes::is_byte_view(ty) => Self::Bytes,
            ty if super::objects::is_object(ty) => Self::Object,
            ty if super::filesystem::is_stats(ty) => Self::Stats,
            ty if super::structured::is_string_array(ty) => Self::StringArray,
            HirType::Array(_) => Self::Array,
            ty if super::decoder::is_decoder(ty) => Self::Decoder,
            HirType::Promise(_) => Self::Promise,
            ty if super::date::is_date(ty) => Self::Date,
            ty if super::http::is_response(ty) => Self::HttpResponse,
            ty if super::time::TimeKind::of(ty) == Some(super::time::TimeKind::Instant) => {
                Self::Instant
            }
            ty if super::time::TimeKind::of(ty) == Some(super::time::TimeKind::PlainDateTime) => {
                Self::PlainDateTime
            }
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
    pub(crate) scalar_number: Func,
    pub(crate) async_result: Func,
}

#[derive(Clone, Copy)]
pub(crate) struct ValueAccessHelpers {
    pub(crate) array_new: Func,
    pub(crate) array_resize: Func,
    pub(crate) has: Func,
    pub(crate) get: Func,
    pub(crate) set: Func,
}

pub(crate) fn emit_access_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    allocator: AllocationFuncs,
    strings: super::strings::StringHelperFuncs,
    objects: super::objects::ObjectHelpers,
    boxed_value: Func,
    string_pool: &super::strings::StringPool,
) -> Result<ValueAccessHelpers> {
    let source = include_str!("values/access.wat")
        .replace("{{array-tag}}", &(ValueTag::Array as u32).to_string())
        .replace(
            "{{length}}",
            &string_pool
                .get("length")
                .expect("dynamic length key")
                .to_string(),
        );
    let functions = runtime::emit_functions(
        module,
        memory,
        &source,
        &BTreeMap::from([
            ("realloc", allocator.realloc),
            ("box", boxed_value),
            ("compare", strings.str_compare),
            ("string-index", strings.str_index),
            ("object-get", objects.get),
            ("object-set", objects.set),
            ("object-dynamic", objects.dynamic),
        ]),
    )?;
    Ok(ValueAccessHelpers {
        has: functions["value.has"],
        array_new: functions["value.array-new"],
        array_resize: functions["value.array-resize"],
        get: functions["value.get"],
        set: functions["value.set"],
    })
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
        scalar_number: functions["value.scalar-number"],
        async_result: functions["value.async-result"],
    })
}

fn contains_dynamic(ty: &HirType) -> bool {
    match ty {
        ty if super::nullable::inner(ty).is_some() => true,
        ty if super::objects::is_object(ty) => true,
        HirType::Promise(inner) | HirType::Array(inner) => contains_dynamic(inner),
        HirType::Tuple(_) => true,
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
