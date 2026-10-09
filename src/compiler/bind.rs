//! Binding WIT world exports to exported TypeScript functions.
use crate::{
    abi::export_names::{core_export_name, interface_implementation_name},
    sdk::codegen::to_camel_case,
};
use anyhow::{Result, bail, ensure};
use perry_codegen::boundary::{AbiType, Export};
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
            ensure!(
                !function.kind.is_async(),
                "WIT export {core_name} is async; async exports need the async split (TODOs.md P3.8)"
            );
            ensure!(
                exported.remove(&implementation).is_some(),
                "WIT export {core_name} needs an exported TypeScript function {implementation}"
            );
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
            bound.push(Export {
                core_name,
                function: implementation,
                params,
                result,
            });
        }
    }
    // Exported functions outside the world stay internal, so one module can serve several
    // worlds (`examples/merge_task.ts` implements both `task-runner` and `merge-task`).
    Ok(bound)
}

fn abi_type(resolve: &Resolve, ty: &Type) -> Result<AbiType> {
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
                TypeDefKind::List(element) => AbiType::List(Box::new(abi_type(resolve, element)?)),
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
