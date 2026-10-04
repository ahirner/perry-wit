//! Typed Temporal identities and imports for the allocation-free Rust time codec.

use perry_hir::{
    ir::{Expr, Module as HirModule},
    types::Type as HirType,
};
use std::collections::BTreeMap;
use waffle::{Func, FuncDecl, Import, ImportKind, Module, SignatureData, Type};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimeKind {
    Instant,
    PlainDateTime,
}

impl TimeKind {
    pub(crate) fn of(ty: &HirType) -> Option<Self> {
        match ty {
            HirType::Named(name) if name == Self::Instant.type_name() => Some(Self::Instant),
            HirType::Named(name) if name == Self::PlainDateTime.type_name() => {
                Some(Self::PlainDateTime)
            }
            _ => None,
        }
    }

    pub(crate) fn type_name(self) -> &'static str {
        match self {
            Self::Instant => "__perry_internal_instant",
            Self::PlainDateTime => "__perry_internal_plain_date_time",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum TimeConstructor {
    InstantFrom,
    InstantFromMs,
    PlainFrom,
}

impl TimeConstructor {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::InstantFrom => "Temporal.Instant.from",
            Self::InstantFromMs => "Temporal.Instant.fromEpochMilliseconds",
            Self::PlainFrom => "Temporal.PlainDateTime.from",
        }
    }

    pub(crate) fn kind(self) -> TimeKind {
        match self {
            Self::InstantFrom | Self::InstantFromMs => TimeKind::Instant,
            Self::PlainFrom => TimeKind::PlainDateTime,
        }
    }

    pub(crate) fn argument_type(self) -> HirType {
        if self == Self::InstantFromMs {
            HirType::Number
        } else {
            HirType::String
        }
    }
}

pub(crate) fn is_time(ty: &HirType) -> bool {
    TimeKind::of(ty).is_some()
}

pub(crate) fn required(hir: &HirModule) -> bool {
    hir.functions.iter().any(|function| {
        let mut required = contains_time(&function.return_type)
            || function.params.iter().any(|param| contains_time(&param.ty));
        super::visit::visit_function_expressions(function, &mut |expression| {
            if let Expr::ExternFuncRef { return_type, .. } = expression {
                required |= is_time(return_type);
            }
        });
        required
    })
}

pub(crate) fn declare_imports(module: &mut Module<'static>) -> BTreeMap<&'static str, Func> {
    [
        ("time_instant_parse", vec![Type::I32; 3], Type::I64),
        (
            "time_instant_from_ms",
            vec![Type::F64, Type::I32],
            Type::I64,
        ),
        ("time_instant_ms", vec![Type::I32], Type::F64),
        ("time_instant_format", vec![Type::I32; 3], Type::I64),
        ("time_plain_parse", vec![Type::I32; 3], Type::I64),
        (
            "time_plain_add_days",
            vec![Type::I32, Type::F64, Type::I32],
            Type::I64,
        ),
        ("time_plain_part", vec![Type::I32; 2], Type::F64),
        ("time_plain_format", vec![Type::I32; 3], Type::I64),
    ]
    .into_iter()
    .map(|(name, params, result)| {
        let signature = module.signatures.push(SignatureData {
            params,
            returns: vec![result],
        });
        let function = module.funcs.push(FuncDecl::Import(signature, name.into()));
        module.imports.push(Import {
            module: super::link::HELPER_MODULE.into(),
            name: name.into(),
            kind: ImportKind::Func(function),
        });
        (name, function)
    })
    .collect()
}

fn contains_time(ty: &HirType) -> bool {
    match ty {
        HirType::Promise(inner) | HirType::Array(inner) => contains_time(inner),
        HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().any(contains_time),
        HirType::Object(fields) => {
            fields
                .properties
                .values()
                .any(|property| contains_time(&property.ty))
                || fields.index_signature.as_deref().is_some_and(contains_time)
        }
        _ => is_time(ty),
    }
}
