//! Binding WIT world exports to exported TypeScript functions.
use super::abi::Layouts;
use perry_codegen::boundary::{AbiKind, Export, ExportKind};
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
        let AbiKind::Scalar(perry_contract::types::Scalar::Integer(domain)) = abi.kind else {
            continue;
        };
        let wide = domain.bits() == 64;
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
            *source = perry_mir::integer_type(ctx, domain).into();
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
) -> Result<Vec<Export>> {
    use perry_frontend::ExportTarget;
    let mut bound = Vec::with_capacity(targets.len());
    for target in targets {
        let (mir_function, entry, annotations, source_name, command) = match target {
            ExportTarget::Destructor(function, core_name) => {
                let export = Export {
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
                };
                bound.push((function, export));
                continue;
            }

            ExportTarget::Function(function, Some(entry), annotations, name) => {
                (function, entry, annotations, name, false)
            }
            ExportTarget::Command(function, entry) => (function, entry, Vec::new(), None, true),
            ExportTarget::Function(_, None, ..) => {
                return Err(Diagnostic::internal("source export has no WIT declaration"));
            }
        };
        let function = entry.function;
        let core_name = entry.core_name(resolve);
        let signature = layouts.signature(function, false);
        let implementation = mir_function.get_symbol_name(ctx).to_string();
        let expected_async = function.kind.is_async();
        if !command && perry_mir::is_async(ctx, &mir_function) != expected_async {
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
        let (params, indirect_size, result) = if command {
            (Vec::new(), None, None)
        } else {
            let (params, indirect_size) = layouts.parameters(function, &signature)?;
            (
                params,
                indirect_size,
                function.result.map(|ty| layouts.value(ty)).transpose()?,
            )
        };
        let kind = if expected_async || command {
            let module = if command || entry.interface.is_some() {
                format!("[export]{}", resolve.name_world_key(entry.key))
            } else {
                "[export]$root".to_owned()
            };
            ExportKind::Async {
                task_return: (module, function.name.clone()),
                completion: if command {
                    perry_codegen::boundary::AsyncCompletion::Command
                } else {
                    perry_codegen::boundary::AsyncCompletion::Function
                },
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
    perry_contract::world_interfaces(&resolve.worlds[world_id].imports)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exported_declarations_survive_reordering() -> anyhow::Result<()> {
        let mut resolve = Resolve::default();
        let package = resolve.push_source("api.wit",
            "package test:order; world api { export left: func(x: u32) -> u32; export right: func() -> string; }")?;
        let world = resolve.select_world(&[package], Some("api"))?;
        let ctx = &mut Context::new();
        let program = perry_frontend::lower_world(
            ctx,
            "export function left(x: number) { return x; } export function right() { return 'r'; }",
            "order.ts",
            &Default::default(),
            &perry_frontend::wit::WorldTypes::new(&resolve, Some(world)),
            Some((&resolve, world)),
        )?;
        let mut targets = program.exports;
        targets.reverse();
        let bound = exports(&Layouts::new(&resolve)?, ctx, targets, &resolve)?;
        assert_eq!(
            bound
                .iter()
                .map(|e| (e.core_name.as_str(), e.params.len()))
                .collect::<Vec<_>>(),
            [("right", 0), ("left", 1)]
        );
        Ok(())
    }
}
