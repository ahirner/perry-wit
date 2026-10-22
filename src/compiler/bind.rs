//! Binding WIT world exports to exported TypeScript functions.
use crate::abi::export_names::core_export_name;
use crate::abi::types::{Contract, Input, convert, primitive};
use anyhow::Result;
use perry_codegen::boundary::{AbiType, Export, ExportKind};
use perry_mir::is_exported;
use pliron::{
    builtin::{
        op_interfaces::SymbolOpInterface,
        ops::{FuncOp, ModuleOp},
    },
    context::Context,
    op::Op,
};
use std::fmt;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

#[derive(Debug)]
enum BindError {
    ArityMismatch {
        span: Option<crate::SourceSpan>,
        function: String,
        actual: usize,
        core_name: String,
        expected: usize,
    },
    WideIntegerRequiresBigInt {
        span: Option<crate::SourceSpan>,
        function: String,
        core_name: String,
    },
    MissingExport {
        span: Option<crate::SourceSpan>,
        core_name: String,
        implementation: String,
        is_unexported: bool,
    },
    AsyncMismatch {
        span: Option<crate::SourceSpan>,
        implementation: String,
        core_name: String,
        is_async: bool,
        expected_async: bool,
    },
    TopLevelRequiresCommandWorld,
    UnsupportedAbiType(String),
}

impl BindError {
    fn into_span(self) -> Option<crate::SourceSpan> {
        match self {
            Self::ArityMismatch { span, .. }
            | Self::WideIntegerRequiresBigInt { span, .. }
            | Self::MissingExport { span, .. }
            | Self::AsyncMismatch { span, .. } => span,
            Self::TopLevelRequiresCommandWorld | Self::UnsupportedAbiType(_) => None,
        }
    }

    fn help(&self) -> Option<String> {
        match self {
            Self::ArityMismatch {
                function,
                expected,
                core_name,
                ..
            } => Some(format!(
                "Update 'export function {function}(...)' to accept {expected} parameter(s) matching WIT export '{core_name}'."
            )),
            Self::WideIntegerRequiresBigInt {
                function,
                core_name,
                ..
            } => Some(format!(
                "Update function '{function}' (implementing WIT export '{core_name}') to use 'bigint' for 64-bit integer parameters and results."
            )),
            Self::MissingExport {
                implementation,
                is_unexported: true,
                ..
            } => Some(format!(
                "Add the 'export' keyword to function '{implementation}'."
            )),
            Self::MissingExport {
                implementation,
                is_unexported: false,
                ..
            } => Some(format!(
                "Export 'export function {implementation}(...)' from your entry module, or run 'perry-wit gen-types' to generate the expected declarations."
            )),
            Self::AsyncMismatch {
                implementation,
                expected_async: true,
                ..
            } => Some(format!(
                "Add 'async' to 'export async function {implementation}(...)' so it matches the WIT export."
            )),
            Self::AsyncMismatch {
                implementation,
                expected_async: false,
                ..
            } => Some(format!(
                "Remove 'async' from 'export function {implementation}(...)' or declare the WIT export 'async'."
            )),
            Self::TopLevelRequiresCommandWorld => Some(
                "Move top-level executable statements into an exported function, or select a command world exporting 'wasi:cli/run'.".to_owned(),
            ),
            Self::UnsupportedAbiType(_) => None,
        }
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ArityMismatch {
                function,
                actual,
                core_name,
                expected,
                ..
            } => write!(
                f,
                "{function} takes {actual} parameters but the WIT export {core_name} has {expected}"
            ),
            Self::WideIntegerRequiresBigInt { .. } => f.write_str(
                "WIT 64-bit integers require TypeScript bigint; use Number(value) for an explicit lossy conversion",
            ),
            Self::MissingExport {
                core_name,
                implementation,
                ..
            } => write!(
                f,
                "WIT export {core_name} needs an exported TypeScript function {implementation}"
            ),
            Self::AsyncMismatch {
                implementation,
                core_name,
                is_async,
                expected_async,
                ..
            } => write!(
                f,
                "{implementation} is {}async but the WIT export {core_name} is {}async",
                if *is_async { "" } else { "not " },
                if *expected_async { "" } else { "not " },
            ),
            Self::TopLevelRequiresCommandWorld => {
                f.write_str("top-level statements need a command world (one exporting wasi:cli/run)")
            }
            Self::UnsupportedAbiType(kind) => {
                write!(f, "WIT {kind} values are not supported by the compiler yet")
            }
        }
    }
}

impl std::error::Error for BindError {}

impl From<BindError> for crate::PerryDiagnostic {
    fn from(error: BindError) -> Self {
        let message = error.to_string();
        let help = error.help();
        let mut diagnostic = Self::error(crate::DiagnosticCode::ExportMismatch, message)
            .with_optional_span(error.into_span());
        if let Some(help) = help {
            diagnostic = diagnostic.with_help(help);
        }
        diagnostic
    }
}

/// Validate source number/bigint distinctions before exposing integer domains to MIR.
pub(crate) fn constrain_numbers(
    ctx: &mut Context,
    mir: &ModuleOp,
    exports: &[Export],
) -> Result<()> {
    use pliron::{
        builtin::{
            type_interfaces::FunctionTypeInterface,
            types::{FP64Type, FunctionType},
        },
        r#type::TypedHandle,
    };
    let functions = module_functions(ctx, mir);
    for export in exports {
        let Some(function) = functions
            .iter()
            .find(|function| function.get_symbol_name(ctx).as_ref() == export.function)
        else {
            continue;
        };
        let existing =
            perry_mir::wit_signature(ctx, function).unwrap_or_else(|| function.get_type(ctx));
        let ty = TypedHandle::<FunctionType>::from_handle(existing, ctx).expect("function type");
        let (mut args, mut results) = (ty.deref(ctx).arg_types(), ty.deref(ctx).res_types());
        if !export.is_command() && args.len() != export.params.len() {
            return Err(crate::PerryDiagnostic::from(BindError::ArityMismatch {
                span: perry_mir::source_span(ctx, function),
                function: export.function.clone(),
                actual: args.len(),
                core_name: export.core_name.clone(),
                expected: export.params.len(),
            })
            .into());
        }
        if args.len() != export.params.len()
            || results.len() != usize::from(export.result.is_some())
        {
            continue;
        }
        let mut constrained = false;
        for (source, abi) in args
            .iter_mut()
            .zip(&export.params)
            .chain(results.iter_mut().zip(export.result.iter()))
        {
            let Some(integer) = abi.integer_type(ctx) else {
                continue;
            };
            let wide = matches!(abi, AbiType::S64 | AbiType::U64);
            let is_wide_int = source
                .deref(ctx)
                .downcast_ref::<pliron::builtin::types::IntegerType>()
                .is_some_and(|int| int.width() == 64);
            if wide && !source.deref(ctx).is::<perry_mir::BigIntType>() && !is_wide_int {
                return Err(
                    crate::PerryDiagnostic::from(BindError::WideIntegerRequiresBigInt {
                        span: perry_mir::source_span(ctx, function),
                        function: export.function.clone(),
                        core_name: export.core_name.clone(),
                    })
                    .into(),
                );
            }
            if (wide && (source.deref(ctx).is::<perry_mir::BigIntType>() || is_wide_int))
                || (!wide
                    && (source.deref(ctx).is::<FP64Type>()
                        || source
                            .deref(ctx)
                            .is::<pliron::builtin::types::IntegerType>()))
            {
                *source = integer;
                constrained = true;
            }
        }
        if constrained {
            perry_mir::constrain_signature(ctx, function, args, results);
        }
    }
    Ok(())
}

pub(crate) fn specialized_implementations(ctx: &Context, mir: &ModuleOp, exports: &mut [Export]) {
    let functions = module_functions(ctx, mir);
    for export in exports {
        if let Some(function) = functions
            .iter()
            .find(|function| function.get_symbol_name(ctx).as_ref() == export.function)
        {
            export.function = perry_mir::specialized_implementation(ctx, function).to_string();
        }
    }
}

/// Bind each component export of `world` to the exported TypeScript function matching the
/// SDK's implementation name. Missing implementations produce errors, and extra exports are ignored.
pub(crate) fn exports(
    ctx: &Context,
    mir: &ModuleOp,
    resolve: &Resolve,
    world_id: WorldId,
) -> Result<Vec<Export>> {
    let world = &resolve.worlds[world_id];
    crate::abi::export_names::validate_implementation_names(resolve, world).map_err(|e| {
        crate::PerryDiagnostic::error(crate::DiagnosticCode::ExportMismatch, e.to_string())
    })?;
    let mut exported = std::collections::HashMap::new();
    let mut main_present = false;
    for function in module_functions(ctx, mir) {
        let name = function.get_symbol_name(ctx);
        main_present |= name.as_ref() == perry_lower::MAIN;
        if is_exported(ctx, &function) {
            exported.insert(name.to_string(), function);
        }
    }

    let mut bound = Vec::new();
    for entry in crate::abi::world_functions(resolve, world.exports.iter()) {
        let function = entry.function;
        let implementation = entry.implementation_name(resolve, world);
        let core_name = if entry.interface.is_some() {
            core_export_name(resolve, entry.key, function)
        } else {
            function.name.clone()
        };
        {
            let found = exported.remove(&implementation);
            // A command's `wasi:cli/run.run` may be the module's top-level statements.
            let command = found.is_none() && is_cli_run(&core_name) && main_present;
            let Some(mir_function) = found else {
                if command {
                    bound.push(Export {
                        core_name,
                        function: perry_lower::MAIN.to_string(),
                        params: vec![],
                        result: None,
                        kind: ExportKind::Async {
                            task_return: (
                                format!("[export]{}", resolve.name_world_key(entry.key)),
                                function.name.clone(),
                            ),
                            command: true,
                        },
                    });
                    continue;
                }
                let unexported = module_functions(ctx, mir).into_iter().find(|candidate| {
                    !is_exported(ctx, candidate)
                        && candidate.get_symbol_name(ctx).as_ref() == implementation
                });
                let span = unexported
                    .as_ref()
                    .and_then(|candidate| perry_mir::source_span(ctx, candidate));
                return Err(crate::PerryDiagnostic::from(BindError::MissingExport {
                    span,
                    core_name,
                    implementation,
                    is_unexported: unexported.is_some(),
                })
                .into());
            };
            let is_async = perry_mir::is_async(ctx, &mir_function);
            if is_async != function.kind.is_async() {
                return Err(crate::PerryDiagnostic::from(BindError::AsyncMismatch {
                    span: perry_mir::source_span(ctx, &mir_function),
                    implementation,
                    core_name,
                    is_async,
                    expected_async: function.kind.is_async(),
                })
                .into());
            }
            let params = function
                .params
                .iter()
                .map(|param| abi_type(resolve, &param.ty))
                .collect::<Result<_>>()?;
            let result = function
                .result
                .as_ref()
                .map(|ty| abi_type(resolve, ty))
                .transpose()?;
            // Async exports return through `task.return`, imported from `[export]<interface>`.
            let kind = if function.kind.is_async() {
                let module = match entry.interface {
                    Some(_) => format!("[export]{}", resolve.name_world_key(entry.key)),
                    None => "[export]$root".to_string(),
                };
                ExportKind::Async {
                    task_return: (module, function.name.clone()),
                    command: false,
                }
            } else {
                ExportKind::Sync
            };
            bound.push(Export {
                core_name,
                function: implementation,
                params,
                result,
                kind,
            });
        }
    }
    // Exported functions outside the selected world stay internal.
    Ok(bound)
}

fn is_cli_run(core_name: &str) -> bool {
    core_name.starts_with("wasi:cli/run@") && core_name.ends_with("#run")
}

fn has_main(ctx: &Context, mir: &ModuleOp) -> bool {
    module_functions(ctx, mir)
        .iter()
        .any(|f| f.get_symbol_name(ctx).as_ref() == perry_lower::MAIN)
}

fn module_functions(ctx: &Context, mir: &ModuleOp) -> Vec<FuncOp> {
    perry_mir::functions(mir.get_operation(), ctx)
}

/// Require a command world (`wasi:cli/run`) when top-level statements are present.
pub(crate) fn check_main(ctx: &Context, mir: &ModuleOp, exports: &[Export]) -> Result<()> {
    if has_main(ctx, mir) && !exports.iter().any(Export::is_command) {
        return Err(crate::PerryDiagnostic::from(BindError::TopLevelRequiresCommandWorld).into());
    }
    Ok(())
}

/// Names of the interfaces `world` imports.
pub(crate) fn imports(resolve: &Resolve, world_id: WorldId) -> Vec<String> {
    resolve.worlds[world_id]
        .imports
        .iter()
        .filter(|(_, item)| matches!(item, WorldItem::Interface { .. }))
        .map(|(key, _)| resolve.name_world_key(key))
        .collect()
}

/// The function of the world whose canonical built-ins serve every `stream<u8>`: an export's
/// first (its stream arguments are the ones read), otherwise an import's.
pub(crate) fn stream_anchor(
    resolve: &Resolve,
    world_id: WorldId,
) -> Option<crate::link::StreamAnchor> {
    let world = &resolve.worlds[world_id];
    stream_functions(resolve, world.exports.iter(), true)
        .chain(stream_functions(resolve, world.imports.iter(), false))
        .next()
}

/// Functions of `items` with a `stream<u8>`, as stream anchors.
fn stream_functions<'a>(
    resolve: &'a Resolve,
    items: impl Iterator<Item = (&'a wit_parser::WorldKey, &'a WorldItem)> + 'a,
    export: bool,
) -> impl Iterator<Item = crate::link::StreamAnchor> + 'a {
    let prefix = if export { "[export]" } else { "" };
    crate::abi::world_functions(resolve, items).filter_map(move |entry| {
        let index = entry
            .function
            .find_futures_and_streams(resolve)
            .iter()
            .position(|id| {
                matches!(resolve.types[*id].kind, TypeDefKind::Stream(Some(Type::U8)))
            })?;
        let module = match entry.interface {
            Some(_) => format!("{prefix}{}", resolve.name_world_key(entry.key)),
            None => format!("{prefix}$root"),
        };
        Some(crate::link::StreamAnchor {
            module,
            function: entry.function.name.clone(),
            index: index as u32,
        })
    })
}

pub(super) fn abi_type(resolve: &Resolve, ty: &Type) -> Result<AbiType> {
    convert(resolve, Input::Reference(*ty), &|_| None, &|contract| {
        Ok(match contract {
            Contract::Scalar(ty) => primitive!(ty, abi).ok_or_else(|| {
                crate::PerryDiagnostic::from(BindError::UnsupportedAbiType(format!("{ty:?}")))
            })?,
            Contract::Resource { id, owned } => AbiType::Resource {
                key: crate::sdk::canonical_type(resolve, id).index() as u32,
                owned,
            },
            Contract::Bytes => AbiType::List(Box::new(AbiType::U8)),
            Contract::ByteStream => AbiType::ByteStream,
            Contract::List(ty) => AbiType::List(Box::new(ty?)),
            Contract::Option(ty) => AbiType::Option(Box::new(ty?)),
            Contract::Result { ok, err } => AbiType::Result {
                ok: ok.transpose()?.map(Box::new),
                err: err.transpose()?.map(Box::new),
            },
            Contract::Tuple(items) => AbiType::Record(
                items
                    .enumerate()
                    .map(|(i, ty)| ty.map(|ty| (format!("_{i}"), ty)))
                    .collect::<Result<_>>()?,
            ),
            Contract::Record(fields) => AbiType::Record(
                fields
                    .map(|(name, ty)| Ok((name, ty?)))
                    .collect::<Result<_>>()?,
            ),
            Contract::Enum(cases) => AbiType::Enum(cases.map(str::to_owned).collect()),
            Contract::Variant(cases) => AbiType::Variant(
                cases
                    .map(|(name, ty)| Ok((name.to_owned(), ty.transpose()?)))
                    .collect::<Result<_>>()?,
            ),
            Contract::Unsupported(kind) => {
                return Err(crate::PerryDiagnostic::from(BindError::UnsupportedAbiType(
                    kind.to_owned(),
                ))
                .into());
            }
        })
    })
}
