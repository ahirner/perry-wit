//! Binding WIT world exports to exported TypeScript functions.
use super::abi::Layouts;
use perry_codegen::boundary::{AbiKind, Export, ExportKind};
use perry_contract::export_names::core_export_name;
use perry_mir::PlironResultExt as _;
use perry_wit_diagnostics::{Diagnostic, Result, diag};
use pliron::{
    builtin::{
        op_interfaces::SymbolOpInterface,
        ops::{FuncOp, ModuleOp},
    },
    context::Context,
    op::Op,
};
use std::collections::HashMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

/// Validate source number/bigint distinctions before exposing integer domains to MIR.
fn constrain_numbers(
    ctx: &mut Context,
    function: &FuncOp,
    export: &Export,
    common_domain: bool,
) -> Result<()> {
    use pliron::{
        builtin::{
            type_interfaces::FunctionTypeInterface,
            types::{FP64Type, FunctionType, IntegerType},
        },
        r#type::TypedHandle,
    };
    let existing =
        perry_mir::wit_signature(ctx, function).unwrap_or_else(|| function.get_type(ctx));
    let ty = TypedHandle::<FunctionType>::from_handle(existing, ctx).to_diag(ctx)?;
    let (mut args, mut results) = (ty.deref(ctx).arg_types(), ty.deref(ctx).res_types());
    if !export.is_command() && args.len() != export.params.len() {
        return Err(diag!(
            crate::codes::CONTRACT_MISMATCH,
            "{} takes {} parameters but the WIT export {} has {}",
            export.declaration(),
            args.len(),
            export.core_name,
            export.params.len()
        )
        .with_optional_span(perry_mir::source_span(ctx, function))
        .with_help(format!(
            "Update 'export function {}(...)' to accept {} parameter(s) matching WIT export '{}'.",
            export.declaration(),
            export.params.len(),
            export.core_name
        )));
    }
    if args.len() != export.params.len() || results.len() != usize::from(export.result.is_some()) {
        return Ok(());
    }
    let mut constrained = false;
    for (index, (source, abi)) in args
        .iter_mut()
        .zip(&export.params)
        .chain(results.iter_mut().zip(export.result.iter()))
        .enumerate()
    {
        let Some(integer) = abi.integer_type(ctx) else {
            continue;
        };
        let wide = matches!(abi.kind, AbiKind::Scalar(Type::S64 | Type::U64));
        let is_wide_int = source
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|int| int.width() == 64);
        if wide && !source.deref(ctx).is::<perry_mir::BigIntType>() && !is_wide_int {
            return Err(Diagnostic::new(
                crate::codes::CONTRACT_MISMATCH,
                "WIT 64-bit integers require TypeScript bigint; use Number(value) for an explicit lossy conversion",
            )
            .with_optional_span(export.annotation(index).or_else(|| perry_mir::source_span(ctx,function)))
            .with_help(format!(
                "Update function '{}' (implementing WIT export '{}') to use 'bigint' for 64-bit integer parameters and results.",
                export.declaration(), export.core_name
            )));
        }
        if common_domain
            && (wide || source.deref(ctx).is::<FP64Type>() || source.deref(ctx).is::<IntegerType>())
        {
            *source = integer;
            constrained = true;
        }
    }
    if constrained {
        perry_mir::constrain_signature(ctx, function, args, results);
    }
    Ok(())
}

pub(super) fn exports(
    layouts: &Layouts,
    ctx: &mut Context,
    targets: Vec<perry_frontend::ExportTarget>,
    resolve: &Resolve,
    world_id: WorldId,
) -> Result<Vec<Export>> {
    let world = &resolve.worlds[world_id];
    let mut targets = targets.into_iter();
    let mut bound = Vec::new();
    for entry in perry_contract::world_functions(resolve, world.exports.iter()) {
        let function = entry.function;
        let target = targets
            .next()
            .ok_or_else(|| Diagnostic::internal("world export has no witness"))?;
        let core_name = if entry.interface.is_some() {
            core_export_name(resolve, entry.key, function)
        } else {
            function.name.clone()
        };
        let signature = layouts.signature(function, false);
        let (mir_function, annotations, source_name) = match target {
            perry_frontend::ExportTarget::Function(func, annotations, name) => {
                (func, annotations, name)
            }
            perry_frontend::ExportTarget::Destructor(..) => {
                return Err(Diagnostic::internal("expected function witness"));
            }
            perry_frontend::ExportTarget::Command(main) => {
                bound.push((
                    main,
                    Export {
                        source_name: None,
                        annotations: Vec::new(),
                        core_name,
                        signature,
                        function: perry_frontend::MAIN.to_string(),
                        params: vec![],
                        indirect_size: None,
                        result: None,
                        kind: ExportKind::Async {
                            task_return: (
                                format!("[export]{}", resolve.name_world_key(entry.key)),
                                function.name.clone(),
                            ),
                            command: true,
                        },
                    },
                ));
                continue;
            }
        };
        let implementation = mir_function.get_symbol_name(ctx).to_string();
        let expected_async = function.kind.is_async();
        if perry_mir::is_async(ctx, &mir_function) != expected_async {
            let implementation = source_name.as_deref().unwrap_or(&implementation);
            let help = if expected_async {
                format!(
                    "Add 'async' to 'export async function {implementation}(...)' so it matches the WIT export."
                )
            } else {
                format!(
                    "Remove 'async' from 'export function {implementation}(...)' or declare the WIT export 'async'."
                )
            };
            return Err(diag!(
                crate::codes::ASYNC_MISMATCH,
                "{implementation} is {}async but the WIT export {core_name} is {}async",
                if expected_async { "not " } else { "" },
                if expected_async { "" } else { "not " },
            )
            .with_optional_span(perry_mir::source_span(ctx, &mir_function))
            .with_help(help));
        }
        let (params, indirect_size) = layouts.parameters(function, &signature)?;
        let result = function.result.map(|ty| layouts.value(ty)).transpose()?;
        // Async exports return through `task.return`, imported from `[export]<interface>`.
        let kind = if expected_async {
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
        bound.push((
            mir_function,
            Export {
                source_name,
                annotations,
                core_name,
                signature,
                function: implementation,
                params,
                indirect_size,
                result,
                kind,
            },
        ));
    }
    for target in targets {
        let perry_frontend::ExportTarget::Destructor(function, core_name) = target else {
            return Err(Diagnostic::internal(
                "unexpected witness after world functions",
            ));
        };
        bound.push((
            function,
            Export {
                source_name: None,
                annotations: Vec::new(),
                core_name,
                signature: wit_parser::abi::WasmSignature {
                    params: vec![wit_parser::abi::WasmType::I32],
                    results: vec![],
                    indirect_params: false,
                    retptr: false,
                },
                function: function.get_symbol_name(ctx).into(),
                params: vec![layouts.value(Type::U32)?],
                indirect_size: None,
                result: None,
                kind: ExportKind::Sync,
            },
        ));
    }
    for (function, export) in &bound {
        let common_domain = bound.iter().all(|(other, abi)| {
            function.get_operation() != other.get_operation()
                || (abi.params == export.params && abi.result == export.result)
        });
        constrain_numbers(ctx, function, export, common_domain)?;
    }
    Ok(bound.into_iter().map(|(_, export)| export).collect())
}

fn module_functions(ctx: &Context, mir: &ModuleOp) -> HashMap<String, FuncOp> {
    perry_mir::functions(mir.get_operation(), ctx)
        .into_iter()
        .map(|function| (function.get_symbol_name(ctx).into(), function))
        .collect()
}

/// Resolve export targets after MIR specialization and check the surviving module initializer.
pub(super) fn finalize_exports(
    ctx: &Context,
    mir: &ModuleOp,
    exports: &mut [Export],
) -> Result<()> {
    let functions = module_functions(ctx, mir);
    if functions.contains_key(perry_frontend::MAIN)
        && !functions.contains_key(perry_mir::INITIALIZER)
        && !exports.iter().any(Export::is_command)
    {
        return Err(Diagnostic::new(
            crate::codes::CONTRACT_MISMATCH,
            "top-level statements need a command world (one exporting wasi:cli/run)",
        )
        .with_help(
            "Move top-level executable statements into an exported function, or select a command world exporting 'wasi:cli/run'.",
        ));
    }
    for export in exports {
        if let Some(function) = functions.get(&export.function) {
            export.function = perry_mir::specialized_implementation(ctx, function).into();
        }
    }
    Ok(())
}

/// Names of the interfaces `world` imports.
pub(super) fn imports(resolve: &Resolve, world_id: WorldId) -> Vec<String> {
    resolve.worlds[world_id]
        .imports
        .iter()
        .filter(|(_, item)| matches!(item, WorldItem::Interface { .. }))
        .map(|(key, _)| resolve.name_world_key(key))
        .collect()
}

/// The function of the world whose canonical built-ins serve every `stream<u8>`: an export's
/// first (its stream arguments are the ones read), otherwise an import's.
pub(super) fn stream_anchor(
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
    perry_contract::world_functions(resolve, items).filter_map(move |entry| {
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
