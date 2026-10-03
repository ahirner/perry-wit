//! Module declarations registry for WAFFLE backend.
//!
//! Provides an immutable registry of complete function and intrinsic metadata
//! before body lowering begins.

use std::collections::BTreeMap;
use anyhow::{bail, Result};
use perry_hir::ir::Module as HirModule;
use perry_hir::types::{FuncId, Type as HirType};
use waffle::{
    Func, FuncDecl, FunctionBody, Import, ImportKind, Module,
    SignatureData, Terminator, Type,
};

use crate::waffle_backend::resolve::ResolvedContract;

/// Calling convention used by a function declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CallingConvention {
    /// Internal module function returning `(status: i32, payload: f64)`.
    Internal,
    /// Exported function returning core Wasm values directly (`[]`, `[f64]`, `[i32]`).
    ExportedDirect,
    /// Exported function returning a WIT Result via memory retptr `[i32]`.
    ExportedWitResult,
}

/// Complete, immutable metadata for a function declaration.
#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub(crate) name: String,
    pub(crate) func_index: Func,
    pub(crate) sig: waffle::Signature,
    pub(crate) return_type: HirType,
    pub(crate) calling_convention: CallingConvention,
    pub(crate) export_name: Option<String>,
}

impl FunctionInfo {
    /// Returns true if the function's logical return type (unwrapping Promise) is Boolean.
    pub(crate) fn is_boolean_return(&self) -> bool {
        let mut t = &self.return_type;
        while let HirType::Promise(inner) = t {
            t = inner.as_ref();
        }
        matches!(t, HirType::Boolean)
    }
}

/// Immutable registry of all module declarations, memory, and intrinsics.
pub(crate) struct ModuleRegistry {
    pub(crate) functions: BTreeMap<FuncId, FunctionInfo>,
    pub(crate) intrinsics: BTreeMap<String, Func>,
    pub(crate) stream_helpers: Option<(Func, Func)>,
    pub(crate) memory: waffle::Memory,
}

impl ModuleRegistry {
    /// Builds the complete module registry and pre-declares all functions in the module.
    pub(crate) fn build(
        module: &mut Module<'static>,
        hir: &HirModule,
        contract: &ResolvedContract,
        stream_helpers: Option<(Func, Func)>,
        memory: waffle::Memory,
    ) -> Result<Self> {
        // 1. Declare async intrinsics as imports
        let mut intrinsics = BTreeMap::new();
        for (name, intrinsic) in &contract.intrinsics {
            let (params, returns) = match intrinsic {
                crate::waffle_backend::resolve::TypedIntrinsic::WaitFor => {
                    (vec![Type::F64], vec![])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::HostDouble => {
                    (vec![Type::F64], vec![Type::F64])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::ReadChunk => {
                    (vec![Type::I32], vec![Type::F64])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::ByteAt => {
                    (vec![Type::F64], vec![Type::F64])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::StreamDrop => {
                    (vec![Type::I32], vec![])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::StreamReset => {
                    (vec![], vec![])
                }
                crate::waffle_backend::resolve::TypedIntrinsic::Custom {
                    params, returns, ..
                } => (params.clone(), returns.clone()),
            };
            let signature = module.signatures.push(SignatureData { params, returns });
            let func = module.funcs.push(FuncDecl::Import(signature, name.clone()));
            module.imports.push(Import {
                module: "host".into(),
                name: name.clone(),
                kind: ImportKind::Func(func),
            });
            intrinsics.insert(name.clone(), func);
        }

        // Add stream reset/drop imports if byte stream contract and not already passed
        let stream_helpers = if let Some(helpers) = stream_helpers {
            Some(helpers)
        } else if contract.input_kind == crate::waffle_backend::resolve::ResolvedInputKind::ByteStream {
            let drop = if let Some(&f) = intrinsics.get("drop") {
                f
            } else {
                let sig = module.signatures.push(SignatureData {
                    params: vec![Type::I32],
                    returns: vec![],
                });
                let f = module.funcs.push(FuncDecl::Import(sig, "drop".into()));
                module.imports.push(Import {
                    module: "host".into(),
                    name: "drop".into(),
                    kind: ImportKind::Func(f),
                });
                f
            };
            let reset = if let Some(&f) = intrinsics.get("reset") {
                f
            } else {
                let sig = module.signatures.push(SignatureData {
                    params: vec![],
                    returns: vec![],
                });
                let f = module.funcs.push(FuncDecl::Import(sig, "reset".into()));
                module.imports.push(Import {
                    module: "host".into(),
                    name: "reset".into(),
                    kind: ImportKind::Func(f),
                });
                f
            };
            Some((drop, reset))
        } else {
            None
        };

        // 2. Pre-declare all functions and establish complete FunctionInfo records
        let mut functions = BTreeMap::new();
        for func in &hir.functions {
            let is_exported = func.is_exported || (func.id == contract.entry_func_id);

            let mut ret_ty = &func.return_type;
            while let HirType::Promise(inner) = ret_ty {
                ret_ty = inner;
            }
            let returns_wit_result = if func.id == contract.entry_func_id {
                contract.entry_returns_wit_result()
            } else {
                matches!(ret_ty, HirType::Generic { base, .. } if base == "Result")
            };

            let calling_convention = if !is_exported {
                CallingConvention::Internal
            } else if returns_wit_result {
                CallingConvention::ExportedWitResult
            } else {
                CallingConvention::ExportedDirect
            };

            let params = func
                .params
                .iter()
                .map(|p| map_type_to_waffle(&p.ty))
                .collect::<Result<Vec<_>>>()?;

            let returns = match calling_convention {
                CallingConvention::Internal => vec![Type::I32, Type::F64],
                CallingConvention::ExportedWitResult => vec![Type::I32],
                CallingConvention::ExportedDirect => map_return_type_to_waffle(&func.return_type)?,
            };

            let sig = module.signatures.push(SignatureData { params, returns });
            let mut body = FunctionBody::new(module, sig);
            body.set_terminator(body.entry, Terminator::Unreachable);
            let func_index = module
                .funcs
                .push(FuncDecl::Body(sig, func.name.clone(), body));

            let export_name = if is_exported {
                let name = if func.name == "main"
                    || func.name == "experiment"
                    || func.id == contract.entry_func_id
                {
                    "run".to_string()
                } else {
                    func.name.clone()
                };
                Some(name)
            } else {
                None
            };

            functions.insert(
                func.id,
                FunctionInfo {
                    name: func.name.clone(),
                    func_index,
                    sig,
                    return_type: func.return_type.clone(),
                    calling_convention,
                    export_name,
                },
            );
        }

        Ok(Self {
            functions,
            intrinsics,
            stream_helpers,
            memory,
        })
    }
}

pub(crate) fn map_type_to_waffle(ty: &HirType) -> Result<Type> {
    match ty {
        HirType::Number | HirType::Any => Ok(Type::F64),
        HirType::Boolean => Ok(Type::I32),
        HirType::Named(name) if name == "ByteStream" => Ok(Type::I32),
        _ => bail!("Unsupported parameter type in WAFFLE lowering: {ty:?}"),
    }
}

pub(crate) fn map_return_type_to_waffle(ty: &HirType) -> Result<Vec<Type>> {
    match ty {
        HirType::Void => Ok(vec![]),
        HirType::Number | HirType::Any => Ok(vec![Type::F64]),
        HirType::Boolean => Ok(vec![Type::I32]),
        HirType::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => {
            Ok(vec![Type::I32])
        }
        HirType::Promise(inner) => map_return_type_to_waffle(inner),
        _ => bail!("Unsupported return type in WAFFLE lowering: {ty:?}"),
    }
}
