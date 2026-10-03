//! Module declarations registry for WAFFLE backend.
//!
//! Provides an immutable registry of complete function and intrinsic metadata
//! before body lowering begins.

use anyhow::{Result, bail, ensure};
use perry_hir::ir::Module as HirModule;
use perry_hir::types::{FuncId, Type as HirType};
use std::collections::BTreeMap;
use waffle::{
    Func, FuncDecl, FunctionBody, Import, ImportKind, Module, SignatureData, Terminator, Type,
};

use crate::waffle_backend::promises::TaskTarget;
use crate::waffle_backend::resolve::ResolvedContract;

/// Host-facing calling convention, separate from the exception-aware guest ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportConvention {
    /// Exported function returning core Wasm values directly (`[]`, `[f64]`, `[i32]`).
    Direct,
    /// Exported function returning a WIT Result via memory retptr `[i32]`.
    WitResult { success: PrimitivePayload },
}

/// Primitive payload representations supported by WIT result adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrimitivePayload {
    Number,
    Boolean,
    String,
}

/// Complete, immutable metadata for a function declaration.
#[derive(Debug, Clone)]
pub(crate) struct FunctionInfo {
    pub(crate) func_index: Func,
    pub(crate) sig: waffle::Signature,
    pub(crate) param_types: Vec<HirType>,
    pub(crate) return_type: HirType,
    pub(crate) export: Option<FunctionExport>,
}

/// A host export wrapper with its own function index and ABI.
#[derive(Debug, Clone)]
pub(crate) struct FunctionExport {
    pub(crate) name: String,
    pub(crate) func_index: Func,
    pub(crate) sig: waffle::Signature,
    pub(crate) convention: ExportConvention,
}

impl FunctionInfo {
    /// Type returned to guest callers after Promise and WIT result transport is removed.
    pub(crate) fn success_type(&self) -> &HirType {
        let mut ty = &self.return_type;
        while let HirType::Promise(inner) = ty {
            ty = inner;
        }
        if let HirType::Generic { base, type_args } = ty
            && base == "Result"
        {
            &type_args[0]
        } else {
            ty
        }
    }
}

/// Immutable registry of all module declarations, memory, and intrinsics.
pub(crate) struct ModuleRegistry {
    pub(crate) promises: Option<PromiseImports>,
    pub(crate) allocator: Option<Func>,
    pub(crate) functions: BTreeMap<FuncId, FunctionInfo>,
    pub(crate) intrinsics: BTreeMap<String, Func>,
    pub(crate) stream_helpers: Option<(Func, Func)>,
    pub(crate) string_helpers: Option<crate::waffle_backend::strings::StringHelperFuncs>,
    pub(crate) memory: waffle::Memory,
}

pub(crate) struct PromiseImports {
    pub(crate) new: Func,
    pub(crate) bind: Func,
    pub(crate) await_result: Func,
    pub(crate) yield_thread: Func,
    pub(crate) starts: BTreeMap<TaskTarget, Func>,
}

impl ModuleRegistry {
    /// Builds the complete module registry and pre-declares all functions in the module.
    pub(crate) fn build(
        module: &mut Module<'static>,
        hir: &HirModule,
        contract: &ResolvedContract,
        stream_helpers: Option<(Func, Func)>,
        string_heap_base: Option<u32>,
        memory: waffle::Memory,
        string_reqs: crate::waffle_backend::strings::RequiredStringHelpers,
    ) -> Result<Self> {
        // 1. Declare async intrinsics as imports
        let mut intrinsics = BTreeMap::new();
        for (name, intrinsic) in &contract.intrinsics {
            let signature = module.signatures.push(intrinsic.core_signature()?);
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
        } else if contract.input_kind
            == crate::waffle_backend::resolve::ResolvedInputKind::ByteStream
        {
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

        let promises = if let Some(plan) = &contract.promises {
            let mut declare = |name: &str, params: Vec<Type>, returns: Vec<Type>| {
                let sig = module.signatures.push(SignatureData { params, returns });
                let func = module.funcs.push(FuncDecl::Import(sig, name.into()));
                module.imports.push(Import {
                    module: "promises".into(),
                    name: name.into(),
                    kind: ImportKind::Func(func),
                });
                func
            };
            let new = declare("new", vec![], vec![Type::I32]);
            let bind = declare("bind", vec![Type::I32, Type::I32], vec![]);
            let await_result = declare("await", vec![Type::I32], vec![Type::I32, Type::F64]);
            let yield_thread = declare("yield", vec![], vec![]);
            let mut starts = BTreeMap::new();
            for (target, task) in &plan.tasks {
                let mut params = vec![Type::I32];
                params.extend(
                    task.params
                        .iter()
                        .map(map_type_to_waffle)
                        .collect::<Result<Vec<_>>>()?,
                );
                starts.insert(
                    target.clone(),
                    declare(&task.symbol, params, vec![Type::I32]),
                );
            }
            Some(PromiseImports {
                new,
                bind,
                await_result,
                yield_thread,
                starts,
            })
        } else {
            None
        };

        // Emit storage helpers only after every function import has been declared.
        let string_helpers =
            if let Some(base) = string_heap_base.filter(|_| string_reqs.needs_strings) {
                Some(crate::waffle_backend::strings::emit_string_runtime(
                    module,
                    memory,
                    base,
                    string_reqs,
                )?)
            } else {
                None
            };
        let allocator = if let Some(helpers) = &string_helpers {
            Some(helpers.cabi_realloc)
        } else if let Some(base) = string_heap_base {
            Some(super::allocation::emit_allocator(module, memory, base)?)
        } else {
            None
        };

        // 3. Pre-declare all functions and establish complete FunctionInfo records
        let mut functions = BTreeMap::new();
        for func in &hir.functions {
            let is_exported = func.is_exported || (func.id == contract.entry_func_id);

            let mut ret_ty = &func.return_type;
            while let HirType::Promise(inner) = ret_ty {
                ret_ty = inner;
            }
            let result_success = if let HirType::Generic { base, type_args } = ret_ty
                && base == "Result"
            {
                ensure!(
                    type_args.len() == 2,
                    "WIT Result requires success and error types"
                );
                ensure!(
                    matches!(type_args[1], HirType::Number | HirType::Any),
                    "WIT Result error payloads must be numeric until the exception ABI preserves primitive type tags"
                );
                Some(match &type_args[0] {
                    HirType::Number | HirType::Any => PrimitivePayload::Number,
                    HirType::Boolean => PrimitivePayload::Boolean,
                    HirType::String => PrimitivePayload::String,
                    other => bail!("Unsupported WIT Result success payload: {other:?}"),
                })
            } else {
                None
            };

            let params = func
                .params
                .iter()
                .map(|p| map_type_to_waffle(&p.ty))
                .collect::<Result<Vec<_>>>()?;

            let host_returns = map_return_type_to_waffle(&func.return_type)?;
            let sig = module.signatures.push(SignatureData {
                params: params.clone(),
                returns: vec![Type::I32, Type::F64],
            });
            let mut body = FunctionBody::new(module, sig);
            body.set_terminator(body.entry, Terminator::Unreachable);
            let func_index = module
                .funcs
                .push(FuncDecl::Body(sig, func.name.clone(), body));

            let export = if is_exported {
                let name = if func.name == "main"
                    || func.name == "experiment"
                    || func.id == contract.entry_func_id
                {
                    "run".to_string()
                } else {
                    func.name.clone()
                };
                let convention = if let Some(success) = result_success {
                    ExportConvention::WitResult { success }
                } else {
                    ExportConvention::Direct
                };
                let param_types: Vec<_> = func.params.iter().map(|p| p.ty.clone()).collect();
                let export_params = canonical_param_types(&param_types)?;
                let sig = module.signatures.push(SignatureData {
                    params: export_params,
                    returns: host_returns,
                });
                let mut body = FunctionBody::new(module, sig);
                body.set_terminator(body.entry, Terminator::Unreachable);
                let func_index =
                    module
                        .funcs
                        .push(FuncDecl::Body(sig, format!("{name}.export"), body));
                Some(FunctionExport {
                    name,
                    func_index,
                    sig,
                    convention,
                })
            } else {
                None
            };

            functions.insert(
                func.id,
                FunctionInfo {
                    func_index,
                    sig,
                    param_types: func.params.iter().map(|p| p.ty.clone()).collect(),
                    return_type: func.return_type.clone(),
                    export,
                },
            );
        }

        Ok(Self {
            promises,
            allocator,
            functions,
            intrinsics,
            stream_helpers,
            string_helpers,
            memory,
        })
    }
}

pub(crate) fn map_type_to_waffle(ty: &HirType) -> Result<Type> {
    match ty {
        HirType::Number | HirType::Any => Ok(Type::F64),
        HirType::Boolean => Ok(Type::I32),
        HirType::String => Ok(Type::I32),
        HirType::Promise(inner)
            if matches!(
                inner.as_ref(),
                HirType::Number | HirType::Boolean | HirType::String | HirType::Void
            ) =>
        {
            Ok(Type::I32)
        }
        HirType::Named(name) if name == "ByteStream" => Ok(Type::I32),
        _ => bail!("Unsupported parameter type in WAFFLE lowering: {ty:?}"),
    }
}

pub(crate) fn map_return_type_to_waffle(ty: &HirType) -> Result<Vec<Type>> {
    match ty {
        HirType::Void => Ok(vec![]),
        HirType::Number | HirType::Any => Ok(vec![Type::F64]),
        HirType::Boolean => Ok(vec![Type::I32]),
        HirType::String => Ok(vec![Type::I32]),
        HirType::Generic { base, type_args } if base == "Result" && type_args.len() == 2 => {
            Ok(vec![Type::I32])
        }
        HirType::Promise(inner) => map_return_type_to_waffle(inner),
        _ => bail!("Unsupported return type in WAFFLE lowering: {ty:?}"),
    }
}

/// Flattens primitive canonical parameters using the same layout as export wrappers.
pub(crate) fn canonical_param_types(params: &[HirType]) -> Result<Vec<Type>> {
    let mut flat = Vec::new();
    for ty in params {
        ensure!(
            !matches!(ty, HirType::Promise(_)),
            "Promise parameters cannot cross the public component boundary"
        );
        flat.push(map_type_to_waffle(ty)?);
        if matches!(ty, HirType::String) {
            flat.push(Type::I32);
        }
    }
    Ok(flat)
}
