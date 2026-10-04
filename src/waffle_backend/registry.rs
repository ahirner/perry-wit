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

use crate::waffle_backend::capabilities::{CapabilityImplementation, LowerCapability};
use crate::waffle_backend::promises::TaskTarget;
use crate::waffle_backend::resolve::{ResolvedContract, TypedIntrinsic};

/// Host-facing calling convention, separate from the exception-aware guest ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportConvention {
    ResolvedWit,
    /// Exported function returning core Wasm values directly (`[]`, `[f64]`, `[i32]`).
    Direct,
    /// Exported function returning a WIT Result via memory retptr `[i32]`.
    WitResult {
        success: ValuePayload,
    },
}

/// Payload representations supported by WIT result adapters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValuePayload {
    Number,
    Boolean,
    String,
    Bytes,
    TextOrBytes,
    Stats,
    StringArray,
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
    pub(crate) finish_command: Option<Func>,
    pub(crate) await_subtask: Option<Func>,
    pub(crate) module_state: Option<super::initialization::ModuleState>,
    pub(crate) promises: Option<PromiseImports>,
    pub(crate) allocator: Option<super::allocation::AllocationFuncs>,
    pub(crate) byte_helpers: Option<super::bytes::ByteHelpers>,
    pub(crate) text_or_bytes_lift: Option<Func>,
    pub(crate) value_helpers: Option<super::values::ValueHelpers>,
    pub(crate) value_access: Option<super::values::ValueAccessHelpers>,
    pub(crate) json_helpers: Option<super::json::JsonHelpers>,
    pub(crate) date_helpers: Option<super::date::DateHelpers>,
    pub(crate) time_helpers: BTreeMap<&'static str, Func>,
    pub(crate) decoder_helpers: Option<super::decoder::DecoderHelpers>,
    pub(crate) http_helpers: Option<super::http::HttpHelpers>,
    pub(crate) filesystem_helpers: Option<super::filesystem::FilesystemHelpers>,
    pub(crate) object_helpers: Option<super::objects::ObjectHelpers>,
    pub(crate) structured_helpers: Option<super::structured::StructuredHelpers>,
    pub(crate) functions: BTreeMap<FuncId, FunctionInfo>,
    pub(crate) intrinsics: BTreeMap<String, Func>,
    pub(crate) stream_helpers: Option<super::streams::StreamHelpers>,
    pub(crate) string_helpers: Option<crate::waffle_backend::strings::StringHelperFuncs>,
    pub(crate) memory: waffle::Memory,
}

pub(crate) struct PromiseImports {
    pub(crate) native: super::promises::native::NativeRuntime,
    pub(crate) new: Func,
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
        string_heap_base: Option<u32>,
        memory: waffle::Memory,
        string_reqs: crate::waffle_backend::strings::RequiredStringHelpers,
        string_pool: &super::strings::StringPool,
    ) -> Result<Self> {
        // 1. Declare async intrinsics as imports
        let mut intrinsics = BTreeMap::new();
        let process_imports = super::capabilities::process::declare(module, contract);
        let scalar_imports = super::capabilities::scalars::declare_imports(module, contract);
        let mut wit_imports = BTreeMap::new();
        for (name, intrinsic) in &contract.intrinsics {
            if scalar_imports.contains_key(name) {
                continue;
            }
            if let TypedIntrinsic::WitImport { key, .. } = intrinsic {
                let wit = contract.wit.as_ref().unwrap();
                let import = &wit.imports[key];
                let signature = wit.resolve.wasm_signature(import.abi(), &import.function);
                let signature = module.signatures.push(SignatureData {
                    params: signature
                        .params
                        .into_iter()
                        .map(super::wit::core_type)
                        .collect(),
                    returns: signature
                        .results
                        .into_iter()
                        .map(super::wit::core_type)
                        .collect(),
                });
                let function = module.funcs.push(FuncDecl::Import(signature, name.clone()));
                module.imports.push(Import {
                    module: import.module.clone(),
                    name: if import.function.kind.is_async() {
                        format!("[async-lower]{}", import.function.name)
                    } else {
                        import.function.name.clone()
                    },
                    kind: ImportKind::Func(function),
                });
                wit_imports.insert(name.clone(), (key.clone(), function));
                continue;
            }
            if matches!(
                intrinsic,
                TypedIntrinsic::ReadChunk
                    | TypedIntrinsic::ReadInto
                    | TypedIntrinsic::ByteAt
                    | TypedIntrinsic::DecoderNew
                    | TypedIntrinsic::DateNew
                    | TypedIntrinsic::Temporal(_)
            ) || matches!(
                intrinsic,
                TypedIntrinsic::Capability(operation)
                    if !matches!(operation.lower().implementation, CapabilityImplementation::Scalar)
            ) {
                continue;
            }
            let signature = module.signatures.push(intrinsic.core_signature()?);
            let func = module.funcs.push(FuncDecl::Import(signature, name.clone()));
            module.imports.push(Import {
                module: "host".into(),
                name: name.clone(),
                kind: ImportKind::Func(func),
            });
            intrinsics.insert(name.clone(), func);
        }

        let subtask_imports = contract
            .intrinsics
            .values()
            .any(TypedIntrinsic::owns_subtask)
            .then(|| super::runtime::subtasks::declare(module));

        let stream_imports = contract
            .has_stream_input()
            .then(|| super::streams::declare_imports(module));

        let output_operations = contract.output_operations();
        let output_imports = (!output_operations.is_empty())
            .then(|| super::streams::output::declare_imports(module, &output_operations));
        let http_imports = contract.has_http().then(|| {
            let mut imports = super::http::declare_imports(module);
            if contract.has_fetch() {
                imports.extend(super::http::fetch::declare_helpers(module));
            }
            imports
        });
        let filesystem_imports = contract
            .has_filesystem()
            .then(|| super::filesystem::declare_imports(module));

        let random_imports = contract
            .random_operations()
            .iter()
            .any(|operation| operation.needs_bytes())
            .then(|| super::random::declare_imports(module));

        let context_operations = contract.context_operations();
        let context_imports = (!context_operations.is_empty())
            .then(|| super::context::declare_imports(module, &context_operations));

        let json_imports = string_reqs
            .json
            .then(|| super::json::declare_imports(module));

        let date_import = (super::date::required(hir) || string_reqs.json)
            .then(|| super::date::declare_import(module));
        let time_helpers = if super::time::required(hir) {
            super::time::declare_imports(module)
        } else {
            BTreeMap::new()
        };

        let string_imports = super::strings::declare_imports(module, string_reqs)?;
        let promises = contract
            .promises
            .as_ref()
            .map(|plan| super::promises::native::declare(module, plan))
            .transpose()?;

        // Emit storage helpers only after every function import has been declared.
        let string_helpers =
            if let Some(base) = string_heap_base.filter(|_| string_reqs.needs_strings) {
                Some(crate::waffle_backend::strings::emit_string_runtime(
                    module,
                    memory,
                    base,
                    string_imports,
                )?)
            } else {
                None
            };
        let allocator = if let Some(helpers) = &string_helpers {
            Some(helpers.allocator)
        } else if let Some(base) = string_heap_base {
            Some(super::allocation::emit_allocator(module, memory, base)?)
        } else {
            None
        };

        let finish_command = if let Some(imports) = process_imports {
            let (helpers, finish) = super::capabilities::process::emit(module, memory, imports)?;
            intrinsics.extend(helpers);
            finish
        } else {
            None
        };
        let await_subtask = subtask_imports
            .map(|imports| {
                super::runtime::subtasks::emit_wait(module, memory, allocator.unwrap(), imports)
            })
            .transpose()?;
        intrinsics.extend(super::capabilities::scalars::emit(
            module,
            contract,
            memory,
            allocator,
            scalar_imports,
            await_subtask,
        )?);

        let byte_helpers = if super::bytes::required(hir) || contract.has_http() {
            Some(super::bytes::emit_runtime(
                module,
                memory,
                allocator.expect("byte storage requires an allocator"),
            )?)
        } else {
            None
        };

        let text_or_bytes_lift = if hir.functions.iter().any(|function| {
            function
                .params
                .iter()
                .any(|param| super::text_or_bytes::is_text_or_bytes(&param.ty))
        }) {
            Some(super::text_or_bytes::emit_lift(
                module,
                memory,
                string_helpers
                    .expect("union values require string helpers")
                    .lift_canonical,
                byte_helpers
                    .expect("union values require byte helpers")
                    .lift_canonical,
            )?)
        } else {
            None
        };

        let date_helpers = if let Some(format) = date_import {
            Some(super::date::emit_runtime(
                module,
                memory,
                allocator.expect("Date storage requires an allocator"),
                format,
            )?)
        } else {
            None
        };
        let decoder_helpers = if string_reqs.decoder {
            Some(super::decoder::emit_runtime(
                module,
                memory,
                allocator.expect("decoder storage requires an allocator"),
            )?)
        } else {
            None
        };

        let value_helpers = if super::values::required(hir) || string_reqs.objects {
            Some(super::values::emit_runtime(
                module,
                memory,
                allocator.expect("dynamic values require storage"),
                string_helpers
                    .expect("dynamic text requires comparison")
                    .str_compare,
            )?)
        } else {
            None
        };
        let object_helpers = if string_reqs.objects {
            Some(super::objects::emit_runtime(
                module,
                memory,
                allocator.expect("objects require an allocator"),
                string_helpers
                    .expect("objects require string keys")
                    .str_compare,
                value_helpers.expect("objects share tagged values").new,
            )?)
        } else {
            None
        };

        let value_access = if let Some(objects) = object_helpers {
            Some(super::values::emit_access_runtime(
                module,
                memory,
                allocator.unwrap(),
                string_helpers.unwrap(),
                objects,
                value_helpers.unwrap().new,
                string_pool,
            )?)
        } else {
            None
        };

        let json_helpers = if let Some(mut imports) = json_imports {
            imports.insert("array-new", value_access.unwrap().array_new);
            imports.insert("date-iso", date_helpers.unwrap().iso);
            Some(super::json::emit_runtime(
                module,
                memory,
                allocator.unwrap(),
                string_helpers.unwrap(),
                object_helpers.unwrap(),
                value_helpers.unwrap().new,
                imports,
            )?)
        } else {
            None
        };

        let structured_helpers = if super::structured::required(hir)
            || context_operations.contains(&super::capabilities::ContextOperation::Arguments)
        {
            Some(super::structured::emit_runtime(
                module,
                memory,
                allocator.expect("structured values require an allocator"),
            )?)
        } else {
            None
        };

        let stream_helpers = if let Some(imports) = stream_imports {
            let helpers = super::streams::emit_runtime(
                module,
                memory,
                allocator.expect("stream buffers require an allocator"),
                imports,
            )?;
            for (name, intrinsic) in &contract.intrinsics {
                let function = match intrinsic {
                    TypedIntrinsic::ReadChunk => helpers.read_chunk,
                    TypedIntrinsic::ReadInto => helpers.read_into,
                    TypedIntrinsic::ByteAt => helpers.byte_at,
                    _ => continue,
                };
                intrinsics.insert(name.clone(), function);
            }
            Some(helpers)
        } else {
            None
        };

        if let Some(imports) = output_imports {
            let helpers = super::streams::output::emit_runtime(
                module,
                memory,
                allocator.expect("output storage requires an allocator"),
                imports,
                &output_operations,
            )?;
            for (name, intrinsic) in &contract.intrinsics {
                if let TypedIntrinsic::Capability(
                    super::capabilities::CapabilityOperation::Stdio(operation),
                ) = intrinsic
                {
                    intrinsics.insert(name.clone(), helpers[operation.name()]);
                }
            }
        }

        if let Some(imports) = random_imports {
            let helpers = super::random::emit_runtime(
                module,
                memory,
                allocator.expect("random byte storage requires an allocator"),
                imports,
            )?;
            for (name, intrinsic) in &contract.intrinsics {
                if let TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Random(
                    operation,
                )) = intrinsic
                    && operation.needs_bytes()
                {
                    intrinsics.insert(name.clone(), helpers[operation.name()]);
                }
            }
        }

        let http_helpers = if let Some(imports) = http_imports {
            Some(super::http::emit_source_runtime(
                module,
                memory,
                allocator.unwrap(),
                &imports,
                string_helpers.unwrap(),
                byte_helpers.unwrap(),
                string_pool,
            )?)
        } else {
            None
        };
        let filesystem_helpers = if let Some(imports) = filesystem_imports {
            Some(super::filesystem::emit_runtime(
                module,
                memory,
                allocator.expect("filesystem storage requires an allocator"),
                imports,
                string_helpers
                    .expect("filesystem paths require strings")
                    .str_compare,
                string_pool,
            )?)
        } else {
            None
        };

        if let Some(helpers) = http_helpers {
            for (name, intrinsic) in &contract.intrinsics {
                if matches!(
                    intrinsic,
                    TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::HttpGet)
                ) {
                    intrinsics.insert(name.clone(), helpers.get);
                }
            }
        }

        if let Some(helpers) = http_helpers {
            for (name, intrinsic) in &contract.intrinsics {
                if matches!(
                    intrinsic,
                    TypedIntrinsic::Capability(super::capabilities::CapabilityOperation::Fetch)
                ) {
                    intrinsics.insert(name.clone(), helpers.fetch.unwrap().fetch);
                }
            }
        }

        if let Some(imports) = context_imports {
            let helpers = super::context::emit_runtime(
                module,
                memory,
                allocator.expect("context requires storage"),
                imports,
                &context_operations,
                super::context::ContextHelpers {
                    string_lift: string_helpers
                        .expect("context requires strings")
                        .lift_canonical,
                    structured: structured_helpers,
                    objects: object_helpers,
                },
            )?;
            for (name, intrinsic) in &contract.intrinsics {
                if let TypedIntrinsic::Capability(
                    super::capabilities::CapabilityOperation::Context(operation),
                ) = intrinsic
                {
                    intrinsics.insert(name.clone(), helpers[operation.name()]);
                }
            }
        }

        // 3. Pre-declare all functions and establish complete FunctionInfo records
        let mut functions = BTreeMap::new();
        for func in &hir.functions {
            let wit_export = contract
                .wit
                .as_ref()
                .and_then(|wit| wit.functions.get(&func.name));
            let is_exported = if contract.wit.is_some() {
                wit_export.is_some()
            } else {
                func.is_exported || (func.id == contract.entry_func_id)
            };

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
                    ty if super::values::is_dynamic(ty) => ValuePayload::Number,
                    HirType::Number | HirType::Any => ValuePayload::Number,
                    HirType::Boolean => ValuePayload::Boolean,
                    HirType::String => ValuePayload::String,
                    ty if super::filesystem::is_stats(ty) => ValuePayload::Stats,
                    ty if super::structured::is_string_array(ty) => ValuePayload::StringArray,
                    ty if super::bytes::is_byte_view(ty) => ValuePayload::Bytes,
                    ty if super::text_or_bytes::is_text_or_bytes(ty) => ValuePayload::TextOrBytes,
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

            if is_exported && contract.wit.is_none() {
                ensure!(
                    canonical_param_types(
                        &func
                            .params
                            .iter()
                            .map(|param| param.ty.clone())
                            .collect::<Vec<_>>()
                    )?
                    .len()
                        <= 16,
                    "Core-only entry functions with more than 16 flattened parameters require a resolved WIT world for indirect canonical parameters"
                );
            }
            let host_returns = if contract.wit.is_none() {
                map_return_type_to_waffle(&func.return_type)?
            } else {
                vec![]
            };
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
                let name = if let Some(export) = wit_export {
                    export.core_name.clone()
                } else if func.name == "main"
                    || func.name == "experiment"
                    || func.id == contract.entry_func_id
                {
                    "run".to_string()
                } else {
                    func.name.clone()
                };
                let convention = if wit_export.is_some() {
                    ExportConvention::ResolvedWit
                } else if let Some(success) = result_success {
                    ExportConvention::WitResult { success }
                } else {
                    ExportConvention::Direct
                };
                let param_types: Vec<_> = func.params.iter().map(|p| p.ty.clone()).collect();
                let signature = if let Some(export) = wit_export {
                    contract.wit.as_ref().unwrap().signature(export)
                } else {
                    SignatureData {
                        params: canonical_param_types(&param_types)?,
                        returns: host_returns,
                    }
                };
                let sig = module.signatures.push(signature);
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

        let module_state = contract
            .initialization
            .as_ref()
            .map(|plan| super::initialization::ModuleState::declare(module, plan))
            .transpose()?;
        let mut registry = Self {
            finish_command,
            await_subtask,
            module_state,
            promises,
            allocator,
            byte_helpers,
            text_or_bytes_lift,
            value_helpers,
            date_helpers,
            time_helpers,
            json_helpers,
            value_access,
            decoder_helpers,
            http_helpers,
            filesystem_helpers,
            object_helpers,
            structured_helpers,
            functions,
            intrinsics,
            stream_helpers,
            string_helpers,
            memory,
        };
        for (name, (key, function)) in wit_imports {
            let wit = contract.wit.as_ref().unwrap();
            let wrapper = super::wit::build_import_wrapper(
                module,
                &registry,
                wit,
                &wit.imports[&key],
                function,
                string_pool,
            )?;
            registry.intrinsics.insert(name, wrapper);
        }
        Ok(registry)
    }
}

pub(crate) fn map_type_to_waffle(ty: &HirType) -> Result<Type> {
    match ty {
        ty if super::nullable::inner(ty).is_some() => Ok(Type::I32),
        ty if super::text_or_bytes::is_text_or_bytes(ty) => Ok(Type::I32),
        ty if super::values::is_boxed(ty) => Ok(Type::I32),
        HirType::Number | HirType::Any => Ok(Type::F64),
        HirType::Boolean | HirType::BigInt => Ok(Type::I32),
        ty if super::values::is_string_type(ty) => Ok(Type::I32),
        HirType::Tuple(_) => Ok(Type::I32),
        HirType::Promise(inner) if super::promises::is_task_outcome(inner) => Ok(Type::I32),
        HirType::Named(name) if name == "ByteStream" => Ok(Type::I32),
        ty if super::bytes::is_byte_view(ty) => Ok(Type::I32),
        ty if super::decoder::is_decoder(ty)
            || super::date::is_date(ty)
            || super::time::is_time(ty)
            || super::http::is_response(ty) =>
        {
            Ok(Type::I32)
        }
        ty if super::objects::is_object(ty) => Ok(Type::I32),
        ty if super::filesystem::is_stats(ty) => Ok(Type::I32),
        HirType::Array(_) => Ok(Type::I32),
        _ => bail!("Unsupported parameter type in WAFFLE lowering: {ty:?}"),
    }
}

pub(crate) fn map_return_type_to_waffle(ty: &HirType) -> Result<Vec<Type>> {
    match ty {
        ty if super::nullable::inner(ty).is_some() => Ok(vec![Type::I32]),
        ty if super::text_or_bytes::is_text_or_bytes(ty) => Ok(vec![Type::I32]),
        HirType::Void => Ok(vec![]),
        ty if super::values::is_dynamic(ty) => Ok(vec![Type::F64]),
        HirType::Number | HirType::Any => Ok(vec![Type::F64]),
        HirType::Boolean | HirType::BigInt => Ok(vec![Type::I32]),
        ty if super::values::is_string_type(ty) => Ok(vec![Type::I32]),
        HirType::Tuple(_) => Ok(vec![Type::I32]),
        ty if super::bytes::is_byte_view(ty) => Ok(vec![Type::I32]),
        ty if super::decoder::is_decoder(ty)
            || super::date::is_date(ty)
            || super::time::is_time(ty)
            || super::http::is_response(ty) =>
        {
            Ok(vec![Type::I32])
        }
        ty if super::objects::is_object(ty) => Ok(vec![Type::I32]),
        ty if super::filesystem::is_stats(ty) => Ok(vec![Type::I32]),
        HirType::Array(_) => Ok(vec![Type::I32]),
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
        if super::values::is_dynamic(ty) {
            flat.push(Type::F64);
            continue;
        }
        if super::filesystem::is_stats(ty) {
            flat.extend([Type::F64, Type::F64, Type::I32]);
            continue;
        }
        if super::text_or_bytes::is_text_or_bytes(ty) {
            flat.extend([Type::I32; 3]);
            continue;
        }
        ensure!(
            !matches!(ty, HirType::Promise(_)),
            "Promise parameters cannot cross the public component boundary"
        );
        flat.push(map_type_to_waffle(ty)?);
        if matches!(ty, HirType::String)
            || super::bytes::is_byte_view(ty)
            || super::structured::is_string_array(ty)
        {
            flat.push(Type::I32);
        }
    }
    Ok(flat)
}
