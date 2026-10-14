//! Binding WIT world exports to exported TypeScript functions.
use crate::{
    abi::export_names::{core_export_name, interface_implementation_name},
    sdk::to_camel_case,
};
use anyhow::{Result, bail, ensure};
use perry_codegen::boundary::{AbiType, Export, ExportKind};
use perry_mir::calls::is_exported;
use pliron::{
    builtin::{
        op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface},
        ops::{FuncOp, ModuleOp},
    },
    context::Context,
    linked_list::ContainsLinkedList,
    operation::Operation,
};
use std::collections::BTreeMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

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
        let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
            .expect("function type");
        let (mut args, mut results) = (ty.deref(ctx).arg_types(), ty.deref(ctx).res_types());
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
            ensure!(
                !wide || source.deref(ctx).is::<perry_mir::types::BigIntType>(),
                "WIT 64-bit integers require TypeScript bigint; use Number(value) for an explicit lossy conversion"
            );
            if (wide && source.deref(ctx).is::<perry_mir::types::BigIntType>())
                || (!wide && source.deref(ctx).is::<FP64Type>())
            {
                *source = integer;
                constrained = true;
            }
        }
        if constrained {
            perry_mir::facts::constrain(ctx, function, args, results);
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
            export.function = perry_mir::facts::implementation(ctx, function).to_string();
        }
    }
}

/// The component exports of `world`, each bound to the exported TypeScript function with the
/// SDK's implementation name. Missing implementations are errors; extra exports are ignored.
pub(crate) fn exports(
    ctx: &Context,
    mir: &ModuleOp,
    resolve: &Resolve,
    world_id: WorldId,
) -> Result<Vec<Export>> {
    let world = &resolve.worlds[world_id];
    crate::abi::export_names::validate_implementation_names(resolve, world)?;
    let mut exported: BTreeMap<String, FuncOp> = mir
        .get_body(ctx, 0)
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .filter(|function| is_exported(ctx, function))
        .map(|function| (function.get_symbol_name(ctx).to_string(), function))
        .collect();

    let mut bound = Vec::new();
    for (key, item) in &world.exports {
        let functions: Vec<_> = match item {
            WorldItem::Function(function) => {
                vec![(
                    to_camel_case(&function.name),
                    function.name.clone(),
                    function,
                )]
            }
            WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                .functions
                .values()
                .map(|function| {
                    (
                        interface_implementation_name(resolve, world, key, function),
                        core_export_name(resolve, key, function),
                        function,
                    )
                })
                .collect(),
            WorldItem::Type { .. } => continue,
        };
        for (implementation, core_name, function) in functions {
            let found = exported.remove(&implementation).is_some();
            // A command's `wasi:cli/run.run` may be the module's top-level statements.
            let command = !found && is_cli_run(&core_name) && has_main(ctx, mir);
            ensure!(
                found || command,
                "WIT export {core_name} needs an exported TypeScript function {implementation}"
            );
            if command {
                bound.push(Export {
                    core_name,
                    function: perry_lower::MAIN.to_string(),
                    params: vec![],
                    result: None,
                    kind: ExportKind::Async {
                        task_return: (
                            format!("[export]{}", resolve.name_world_key(key)),
                            function.name.clone(),
                        ),
                        command: true,
                    },
                });
                continue;
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
                let module = match item {
                    WorldItem::Interface { .. } => {
                        format!("[export]{}", resolve.name_world_key(key))
                    }
                    _ => "[export]$root".to_string(),
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
    mir.get_body(ctx, 0)
        .deref(ctx)
        .iter(ctx)
        .filter_map(|op| Operation::get_op::<FuncOp>(op, ctx))
        .collect()
}

/// Top-level statements only run as a command; elsewhere they are an error, not dead code.
pub(crate) fn check_main(ctx: &Context, mir: &ModuleOp, exports: &[Export]) -> Result<()> {
    ensure!(
        !has_main(ctx, mir) || exports.iter().any(Export::is_command),
        "top-level statements need a command world (one exporting wasi:cli/run)"
    );
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
    items.flat_map(move |(key, item)| {
        let (module, functions): (String, Vec<&wit_parser::Function>) = match item {
            WorldItem::Function(function) => (
                if export { "[export]$root" } else { "$root" }.to_string(),
                vec![function],
            ),
            WorldItem::Interface { id, .. } => (
                format!(
                    "{}{}",
                    if export { "[export]" } else { "" },
                    resolve.name_world_key(key)
                ),
                resolve.interfaces[*id].functions.values().collect(),
            ),
            WorldItem::Type { .. } => (String::new(), Vec::new()),
        };
        functions.into_iter().filter_map(move |function| {
            let index = function
                .find_futures_and_streams(resolve)
                .iter()
                .position(|id| {
                    matches!(resolve.types[*id].kind, TypeDefKind::Stream(Some(Type::U8)))
                })?;
            Some(crate::link::StreamAnchor {
                module: module.clone(),
                function: function.name.clone(),
                index: index as u32,
            })
        })
    })
}

pub(super) fn abi_type(resolve: &Resolve, ty: &Type) -> Result<AbiType> {
    Ok(match ty {
        Type::Bool => AbiType::Bool,
        Type::U8 => AbiType::U8,
        Type::U16 => AbiType::U16,
        Type::U32 => AbiType::U32,
        Type::U64 => AbiType::U64,
        Type::S8 => AbiType::S8,
        Type::S16 => AbiType::S16,
        Type::S32 => AbiType::S32,
        Type::S64 => AbiType::S64,
        Type::F32 => AbiType::F32,
        Type::F64 => AbiType::F64,
        Type::String => AbiType::String,
        Type::Id(id) => {
            let definition = &resolve.types[*id];
            match &definition.kind {
                TypeDefKind::Type(inner) => abi_type(resolve, inner)?,
                TypeDefKind::Resource => AbiType::Resource {
                    key: super::resources::canonical(resolve, *id).index() as u32,
                    owned: true,
                },
                TypeDefKind::Handle(
                    wit_parser::Handle::Own(resource) | wit_parser::Handle::Borrow(resource),
                ) => AbiType::Resource {
                    key: super::resources::canonical(resolve, *resource).index() as u32,
                    owned: matches!(
                        definition.kind,
                        TypeDefKind::Handle(wit_parser::Handle::Own(_))
                    ),
                },
                TypeDefKind::List(element) => AbiType::List(Box::new(abi_type(resolve, element)?)),
                TypeDefKind::Option(inner) => AbiType::Option(Box::new(abi_type(resolve, inner)?)),
                TypeDefKind::Result(result) => AbiType::Result {
                    ok: result
                        .ok
                        .as_ref()
                        .map(|ty| abi_type(resolve, ty).map(Box::new))
                        .transpose()?,
                    err: result
                        .err
                        .as_ref()
                        .map(|ty| abi_type(resolve, ty).map(Box::new))
                        .transpose()?,
                },
                TypeDefKind::Variant(variant) => AbiType::Variant(
                    variant
                        .cases
                        .iter()
                        .map(|case| {
                            Ok((
                                case.name.clone(),
                                case.ty
                                    .as_ref()
                                    .map(|ty| abi_type(resolve, ty))
                                    .transpose()?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                ),
                TypeDefKind::Enum(enumeration) => AbiType::Enum(
                    enumeration
                        .cases
                        .iter()
                        .map(|case| case.name.clone())
                        .collect(),
                ),
                TypeDefKind::Tuple(tuple) => AbiType::Record(
                    tuple
                        .types
                        .iter()
                        .enumerate()
                        .map(|(i, ty)| Ok((format!("_{i}"), abi_type(resolve, ty)?)))
                        .collect::<Result<_>>()?,
                ),
                TypeDefKind::Stream(Some(Type::U8)) => AbiType::ByteStream,
                TypeDefKind::Record(record) => AbiType::Record(
                    record
                        .fields
                        .iter()
                        .map(|field| {
                            Ok((to_camel_case(&field.name), abi_type(resolve, &field.ty)?))
                        })
                        .collect::<Result<_>>()?,
                ),
                other => bail!(
                    "WIT {} values are not supported by the compiler yet",
                    other.as_str()
                ),
            }
        }
        Type::Char | Type::ErrorContext => {
            bail!("WIT {ty:?} values are not supported by the compiler yet")
        }
    })
}
