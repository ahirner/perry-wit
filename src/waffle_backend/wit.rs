//! Resolved WIT exports use the SDK's source names and the canonical ABI layouts.

mod adapter;

use anyhow::{Context, Result, bail, ensure};
use perry_hir::{
    ir::Module as HirModule,
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use perry_parser::swc_ecma_ast as ast;
use std::collections::BTreeMap;
use swc_ecma_visit::{Visit, VisitWith};
use waffle::{SignatureData, Type as CoreType};
use wit_parser::{Function, FunctionKind, Resolve, Type, TypeDefKind, WorldId, WorldItem};

use super::strings::StringPool;
use crate::{abi::export_names, sdk::codegen::to_camel_case};

pub(super) use adapter::build_export_wrapper;

pub(super) fn validate_source(module: &ast::Module) -> Result<()> {
    struct ExplicitAny(bool);
    impl Visit for ExplicitAny {
        fn visit_ts_keyword_type(&mut self, ty: &ast::TsKeywordType) {
            self.0 |= ty.kind == ast::TsKeywordTypeKind::TsAnyKeyword;
        }
    }
    let mut any = ExplicitAny(false);
    module.visit_with(&mut any);
    ensure!(
        !any.0,
        "Unconstrained any is unsupported in resolved WIT implementations; use declared types or finite unions"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct WitExports {
    pub(super) resolve: Resolve,
    pub(super) world: WorldId,
    pub(super) functions: BTreeMap<String, WitExport>,
}

#[derive(Clone, Debug)]
pub(crate) struct WitExport {
    pub(super) core_name: String,
    pub(super) function: Function,
}

impl WitExports {
    pub(super) fn new(resolve: Resolve, world: WorldId) -> Result<Self> {
        let contract = &resolve.worlds[world];
        ensure!(
            contract
                .imports
                .values()
                .all(|item| matches!(item, WorldItem::Type { .. })),
            "Resolved WIT imports are not yet supported by the WAFFLE world compiler"
        );
        let mut functions = BTreeMap::new();
        for (key, item) in &contract.exports {
            match item {
                WorldItem::Function(function) => {
                    functions.insert(
                        to_camel_case(&function.name),
                        WitExport {
                            core_name: function.name.clone(),
                            function: function.clone(),
                        },
                    );
                }
                WorldItem::Interface { id, .. } => {
                    for function in resolve.interfaces[*id].functions.values() {
                        functions.insert(
                            export_names::interface_implementation_name(
                                &resolve, contract, key, function,
                            ),
                            WitExport {
                                core_name: export_names::core_export_name(&resolve, key, function),
                                function: function.clone(),
                            },
                        );
                    }
                }
                WorldItem::Type { .. } => {}
            }
        }
        ensure!(!functions.is_empty(), "WIT world must export a function");
        for export in functions.values() {
            ensure!(
                export.function.kind == FunctionKind::Freestanding,
                "Resolved WIT exports currently require synchronous freestanding functions"
            );
            for param in &export.function.params {
                hir_type(&resolve, param.ty)?;
            }
            if let Some(ty) = export.function.result {
                hir_type(&resolve, ty)?;
            }
        }
        Ok(Self {
            resolve,
            world,
            functions,
        })
    }

    pub(super) fn validate(&self, hir: &HirModule) -> Result<()> {
        for function in &hir.functions {
            ensure!(
                static_type(&function.return_type)
                    && function.params.iter().all(|param| static_type(&param.ty)),
                "Function '{}' needs statically known parameter and result types",
                function.name
            );
        }
        for (name, export) in &self.functions {
            let function = hir
                .functions
                .iter()
                .find(|function| function.name == *name && function.is_exported)
                .with_context(|| {
                    format!("WIT export requires TypeScript implementation '{name}'")
                })?;
            ensure!(
                function.params.len() == export.function.params.len(),
                "WIT export '{name}' has the wrong parameter count"
            );
            for (source, wit) in function.params.iter().zip(&export.function.params) {
                let expected = hir_type(&self.resolve, wit.ty)?;
                ensure!(
                    same_type(&source.ty, &expected),
                    "WIT export '{name}' parameter '{}' must match {expected:?}, found {:?}",
                    wit.name,
                    source.ty
                );
            }
            let expected = export
                .function
                .result
                .map(|ty| hir_type(&self.resolve, ty))
                .transpose()?
                .unwrap_or(HirType::Void);
            ensure!(
                same_type(&function.return_type, &expected),
                "WIT export '{name}' result must match {expected:?}, found {:?}",
                function.return_type
            );
        }
        Ok(())
    }

    pub(super) fn intern_keys(&self, pool: &mut StringPool) {
        for export in self.functions.values() {
            for param in &export.function.params {
                self.intern_type(param.ty, pool);
            }
            if let Some(ty) = export.function.result {
                self.intern_type(ty, pool);
            }
        }
    }

    fn intern_type(&self, ty: Type, pool: &mut StringPool) {
        let Type::Id(id) = ty else {
            return;
        };
        match &self.resolve.types[id].kind {
            TypeDefKind::Type(ty) => self.intern_type(*ty, pool),
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    pool.intern(&to_camel_case(&field.name));
                    self.intern_type(field.ty, pool);
                }
            }
            TypeDefKind::Tuple(tuple) => {
                for ty in &tuple.types {
                    self.intern_type(*ty, pool);
                }
            }
            TypeDefKind::Enum(enumeration) => {
                for case in &enumeration.cases {
                    pool.intern(&case.name);
                }
            }
            TypeDefKind::Result(result) => {
                for name in ["ok", "value", "error"] {
                    pool.intern(name);
                }
                for ty in result.ok.iter().chain(&result.err) {
                    self.intern_type(*ty, pool);
                }
            }
            _ => {}
        }
    }

    pub(super) fn signature(&self, export: &WitExport) -> SignatureData {
        let signature = self
            .resolve
            .wasm_signature(wit_parser::abi::AbiVariant::GuestExport, &export.function);
        SignatureData {
            params: signature.params.into_iter().map(core_type).collect(),
            returns: signature.results.into_iter().map(core_type).collect(),
        }
    }

    pub(super) fn frame(&self, core: &[u8]) -> Result<(String, Vec<u8>)> {
        let mut core = core.to_vec();
        wit_component::embed_component_metadata(
            &mut core,
            &self.resolve,
            self.world,
            wit_component::StringEncoding::UTF8,
            false,
        )?;
        let component = wit_component::ComponentEncoder::default()
            .module(&core)?
            .validate(true)
            .encode()?;
        Ok((wasmprinter::print_bytes(&component)?, component))
    }
}

fn core_type(ty: wit_parser::abi::WasmType) -> CoreType {
    use wit_parser::abi::WasmType;
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => CoreType::I32,
        WasmType::I64 | WasmType::PointerOrI64 => CoreType::I64,
        WasmType::F32 => CoreType::F32,
        WasmType::F64 => CoreType::F64,
    }
}

fn record(fields: impl IntoIterator<Item = (String, HirType)>) -> HirType {
    HirType::Object(ObjectType {
        properties: fields
            .into_iter()
            .map(|(name, ty)| {
                (
                    name,
                    PropertyInfo {
                        ty,
                        optional: false,
                        readonly: false,
                    },
                )
            })
            .collect(),
        ..Default::default()
    })
}

fn hir_type(resolve: &Resolve, ty: Type) -> Result<HirType> {
    Ok(match ty {
        Type::Bool => HirType::Boolean,
        Type::U8
        | Type::S8
        | Type::U16
        | Type::S16
        | Type::U32
        | Type::S32
        | Type::F32
        | Type::F64 => HirType::Number,
        Type::String => HirType::String,
        Type::Id(id) => match &resolve.types[id].kind {
            TypeDefKind::Type(ty) => return hir_type(resolve, *ty),
            TypeDefKind::Tuple(tuple) => HirType::Tuple(
                tuple
                    .types
                    .iter()
                    .map(|ty| hir_type(resolve, *ty))
                    .collect::<Result<_>>()?,
            ),
            TypeDefKind::Record(fields) => record(
                fields
                    .fields
                    .iter()
                    .map(|field| Ok((to_camel_case(&field.name), hir_type(resolve, field.ty)?)))
                    .collect::<Result<Vec<_>>>()?,
            ),
            TypeDefKind::Enum(enumeration) => HirType::Union(
                enumeration
                    .cases
                    .iter()
                    .map(|case| HirType::StringLiteral(case.name.clone()))
                    .collect(),
            ),
            TypeDefKind::Result(result) => HirType::Union(vec![
                record(
                    std::iter::once(Ok(("ok".into(), HirType::Boolean)))
                        .chain(
                            result
                                .ok
                                .map(|ty| hir_type(resolve, ty).map(|ty| ("value".into(), ty))),
                        )
                        .collect::<Result<Vec<_>>>()?,
                ),
                record(
                    std::iter::once(Ok(("ok".into(), HirType::Boolean)))
                        .chain(
                            result
                                .err
                                .map(|ty| hir_type(resolve, ty).map(|ty| ("error".into(), ty))),
                        )
                        .collect::<Result<Vec<_>>>()?,
                ),
            ]),
            other => bail!("Unsupported resolved WIT boundary type: {other:?}"),
        },
        other => bail!("Unsupported resolved WIT boundary type: {other:?}"),
    })
}

fn same_type(actual: &HirType, expected: &HirType) -> bool {
    match (actual, expected) {
        (HirType::Object(actual), HirType::Object(expected)) => {
            actual.index_signature.is_none()
                && actual.properties.len() == expected.properties.len()
                && expected.properties.iter().all(|(key, field)| {
                    actual
                        .properties
                        .get(key)
                        .is_some_and(|found| !found.optional && same_type(&found.ty, &field.ty))
                })
        }
        (HirType::Tuple(actual), HirType::Tuple(expected)) => {
            actual.len() == expected.len()
                && actual.iter().zip(expected).all(|(a, b)| same_type(a, b))
        }
        (HirType::Union(actual), HirType::Union(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .all(|a| expected.iter().any(|b| same_type(a, b)))
        }
        _ => actual == expected,
    }
}

fn static_type(ty: &HirType) -> bool {
    match ty {
        HirType::Any | HirType::Unknown => false,
        HirType::Object(record) => {
            record
                .properties
                .values()
                .all(|property| static_type(&property.ty))
                && record.index_signature.as_deref().is_none_or(static_type)
        }
        HirType::Array(inner) | HirType::Promise(inner) => static_type(inner),
        HirType::Tuple(types)
        | HirType::Union(types)
        | HirType::Generic {
            type_args: types, ..
        } => types.iter().all(static_type),
        _ => true,
    }
}
