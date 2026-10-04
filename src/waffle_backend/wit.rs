//! Resolved WIT worlds use the SDK's source names and canonical ABI layouts.

mod adapter;
mod native;
mod source;

use anyhow::{Context, Result, bail, ensure};
use perry_hir::{
    ir::Module as HirModule,
    types::{ObjectType, PropertyInfo, Type as HirType},
};
use perry_parser::swc_ecma_ast as ast;
use std::collections::{BTreeMap, BTreeSet};
use swc_ecma_visit::{Visit, VisitWith};
use waffle::{SignatureData, Type as CoreType};
use wit_parser::{Function, FunctionKind, Resolve, Type, TypeDefKind, WorldId, WorldItem};

use super::strings::StringPool;
use crate::{abi::export_names, sdk::codegen::to_camel_case};

pub(super) use adapter::{build_export_wrapper, build_import_wrapper};

pub(super) fn validate_source(module: &ast::Module) -> Result<()> {
    for item in &module.body {
        match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                match &export.decl {
                    ast::Decl::Fn(function) => {
                        ensure!(
                            !function.declare && function.function.body.is_some(),
                            "Component exports require function implementations"
                        );
                        ensure!(
                            !function.function.is_generator
                                && function.function.type_params.is_none(),
                            "Component exports require non-generic functions without generators"
                        );
                        ensure!(
                            function
                                .function
                                .params
                                .iter()
                                .all(|param| matches!(param.pat, ast::Pat::Ident(_))),
                            "Component export parameters require named identifiers without defaults or rest parameters"
                        );
                    }
                    ast::Decl::TsInterface(_) | ast::Decl::TsTypeAlias(_) => {}
                    _ => bail!("Component exports require named function declarations"),
                }
            }
            ast::ModuleItem::ModuleDecl(
                ast::ModuleDecl::ExportDefaultDecl(_)
                | ast::ModuleDecl::ExportDefaultExpr(_)
                | ast::ModuleDecl::TsExportAssignment(_),
            ) => bail!("Component exports require named function declarations"),
            _ => {}
        }
    }
    struct ExplicitAny(bool);
    impl Visit for ExplicitAny {
        fn visit_ts_keyword_type(&mut self, ty: &ast::TsKeywordType) {
            self.0 |= ty.kind == ast::TsKeywordTypeKind::TsAnyKeyword;
        }
    }
    struct Reserved(bool);
    impl Visit for Reserved {
        fn visit_ident(&mut self, ident: &ast::Ident) {
            self.0 |= ident.sym.starts_with("__perry_wit_import_");
        }
    }
    let mut reserved = Reserved(false);
    module.visit_with(&mut reserved);
    ensure!(!reserved.0, "Reserved WIT binding name in source");
    let mut any = ExplicitAny(false);
    module.visit_with(&mut any);
    ensure!(
        !any.0,
        "Unconstrained any is unsupported in resolved WIT implementations; use declared types or finite unions"
    );
    Ok(())
}

#[derive(Clone, Debug)]
pub(crate) struct WitWorld {
    pub(super) resolve: Resolve,
    pub(super) world: WorldId,
    pub(super) functions: BTreeMap<String, WitExport>,
    pub(super) imports: BTreeMap<String, WitImport>,
}

#[derive(Clone, Debug)]
pub(crate) struct WitImport {
    pub(super) module: String,
    pub(super) function: Function,
}

impl WitImport {
    pub(crate) fn abi(&self) -> wit_parser::abi::AbiVariant {
        if self.function.kind.is_async() {
            wit_parser::abi::AbiVariant::GuestImportAsync
        } else {
            wit_parser::abi::AbiVariant::GuestImport
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct WitExport {
    pub(super) core_name: String,
    pub(super) function: Function,
}

impl WitWorld {
    pub(super) fn new(resolve: Resolve, world: WorldId) -> Result<Self> {
        let resolved = Self::for_encoding(resolve, world)?;
        for export in resolved.functions.values() {
            ensure!(
                matches!(
                    export.function.kind,
                    FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
                ),
                "Resolved WIT exports require freestanding functions"
            );
            for param in &export.function.params {
                hir_type(&resolved.resolve, param.ty)?;
            }
            if let Some(ty) = export.function.result {
                hir_type(&resolved.resolve, ty)?;
            }
        }
        Ok(resolved)
    }

    pub(super) fn for_encoding(resolve: Resolve, world: WorldId) -> Result<Self> {
        let contract = &resolve.worlds[world];
        export_names::validate_implementation_names(&resolve, contract)?;
        let mut imports = BTreeMap::new();
        for (key, item) in &contract.imports {
            match item {
                WorldItem::Interface { id, .. } => {
                    let module = resolve.name_world_key(key);
                    for function in resolve.interfaces[*id].functions.values() {
                        imports.insert(
                            format!("{module}#{}", function.name),
                            WitImport {
                                module: module.clone(),
                                function: function.clone(),
                            },
                        );
                    }
                }
                WorldItem::Function(function) => {
                    imports.insert(
                        function.name.clone(),
                        WitImport {
                            module: "$root".into(),
                            function: function.clone(),
                        },
                    );
                }
                WorldItem::Type { .. } => {}
            }
        }
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
        Ok(Self {
            resolve,
            world,
            functions,
            imports,
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
            let result = match &function.return_type {
                HirType::Promise(inner) if function.is_async => inner.as_ref(),
                ty => ty,
            };
            ensure!(
                same_type(result, &expected),
                "WIT export '{name}' result must match {expected:?}, found {:?}",
                function.return_type
            );
        }
        Ok(())
    }

    pub(super) fn validate_suspension(
        &self,
        hir: &HirModule,
        contract: &super::resolve::ResolvedContract,
    ) -> Result<()> {
        use super::{capabilities::CapabilityOperation, resolve::TypedIntrinsic};
        use perry_hir::ir::Expr;
        let functions: BTreeMap<_, _> = hir
            .functions
            .iter()
            .map(|function| (function.id, function))
            .collect();
        for (name, export) in &self.functions {
            if matches!(export.function.kind, FunctionKind::AsyncFreestanding) {
                continue;
            }
            let mut pending = vec![contract.functions_by_name[name]];
            if let Some(initialization) = &contract.initialization {
                pending.push(initialization.function);
            }
            let mut visited = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if !visited.insert(id) {
                    continue;
                }
                let mut suspending = None;
                super::visit::visit_statements(&functions[&id].body, &mut |expression| {
                    match expression {
                        Expr::Await(_)
                            if contract
                                .initialization
                                .as_ref()
                                .is_some_and(|initialization| initialization.function == id) =>
                        {
                            suspending = Some("module initialization".to_owned());
                        }
                        Expr::FuncRef(callee) => pending.push(*callee),
                        Expr::ExternFuncRef { name, .. } => {
                            if let Some(intrinsic) = contract.intrinsics.get(name)
                                && (intrinsic.is_async()
                                    || matches!(
                                        intrinsic,
                                        TypedIntrinsic::Capability(
                                            CapabilityOperation::Filesystem(_)
                                                | CapabilityOperation::Stdio(_)
                                        )
                                    ))
                            {
                                suspending = Some(intrinsic.name().to_owned());
                            }
                        }
                        _ => {}
                    }
                });
                if let Some(operation) = suspending {
                    bail!(
                        "WIT export '{name}' can suspend through '{operation}'; declare it as 'async func' in WIT"
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn intern_keys(&self, pool: &mut StringPool, used: impl Iterator<Item = String>) {
        for name in used {
            let function = &self.imports[&name].function;
            for param in &function.params {
                self.intern_type(param.ty, pool);
            }
            if let Some(ty) = function.result {
                self.intern_type(ty, pool);
            }
        }
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
            TypeDefKind::Flags(flags) => {
                for flag in &flags.flags {
                    pool.intern(&to_camel_case(&flag.name));
                }
            }
            TypeDefKind::List(inner) => self.intern_type(*inner, pool),
            TypeDefKind::Type(ty) => self.intern_type(*ty, pool),
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    pool.intern(&to_camel_case(&field.name));
                    self.intern_type(field.ty, pool);
                }
            }
            TypeDefKind::Option(inner) => self.intern_type(*inner, pool),
            TypeDefKind::Variant(variant) => {
                pool.intern("tag");
                pool.intern("val");
                for case in &variant.cases {
                    pool.intern(&case.name);
                    if let Some(ty) = case.ty {
                        self.intern_type(ty, pool);
                    }
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
        let component = crate::component::encode_resolved(core, &self.resolve, self.world)?;
        Ok((wasmprinter::print_bytes(&component)?, component))
    }
}

pub(super) fn core_type(ty: wit_parser::abi::WasmType) -> CoreType {
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
        Type::U64 => HirType::BigInt,
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
            TypeDefKind::Flags(flags) => {
                ensure!(
                    flags.flags.len() <= 32,
                    "WIT flags currently support at most 32 fields"
                );
                let HirType::Object(mut object) = record(
                    flags
                        .flags
                        .iter()
                        .map(|flag| (to_camel_case(&flag.name), HirType::Boolean)),
                ) else {
                    unreachable!()
                };
                for field in object.properties.values_mut() {
                    field.optional = true;
                }
                HirType::Object(object)
            }
            TypeDefKind::List(Type::U8) => HirType::Named("Uint8Array".into()),
            TypeDefKind::List(inner) => HirType::Array(Box::new(hir_type(resolve, *inner)?)),
            TypeDefKind::Type(ty) => return hir_type(resolve, *ty),
            TypeDefKind::Option(inner) => {
                let inner = hir_type(resolve, *inner)?;
                ensure!(
                    super::nullable::inner(&inner).is_none(),
                    "Nested WIT options need a distinct presence representation"
                );
                HirType::Union(vec![inner, HirType::Null, HirType::Void])
            }
            TypeDefKind::Variant(variant) => HirType::Union(
                variant
                    .cases
                    .iter()
                    .map(|case| {
                        let mut fields =
                            vec![("tag".into(), HirType::StringLiteral(case.name.clone()))];
                        if let Some(ty) = case.ty {
                            fields.push(("val".into(), hir_type(resolve, ty)?));
                        }
                        Ok(record(fields))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
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

pub(super) fn same_type(actual: &HirType, expected: &HirType) -> bool {
    match (actual, expected) {
        (HirType::Array(actual), HirType::Array(expected)) => same_type(actual, expected),
        (HirType::Object(actual), HirType::Object(expected)) => {
            actual.index_signature.is_none()
                && actual.properties.len() == expected.properties.len()
                && expected.properties.iter().all(|(key, field)| {
                    actual.properties.get(key).is_some_and(|found| {
                        found.optional == field.optional && same_type(&found.ty, &field.ty)
                    })
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
