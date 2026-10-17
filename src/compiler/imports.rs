//! World imports described once for source typing and canonical lowering.
use super::{bind::abi_type, wit_types};
use crate::sdk::to_camel_case;
use perry_codegen::boundary::Import;
use perry_lower::hir::{Foreign, FunctionType, Type as HirType};
use std::collections::HashMap;
use wit_parser::{Resolve, WorldId, WorldItem};

pub(super) fn bindings(
    resolve: &Resolve,
    world: WorldId,
    foreign: &mut HashMap<String, Foreign>,
) -> Vec<Import> {
    let mut imports = Vec::new();
    for (key, item) in &resolve.worlds[world].imports {
        let (module, functions): (String, Vec<_>) = match item {
            WorldItem::Interface { id, .. } => (
                resolve.name_world_key(key),
                resolve.interfaces[*id].functions.values().collect(),
            ),
            WorldItem::Function(function) => ("$root".into(), vec![function]),
            WorldItem::Type { .. } => continue,
        };
        for function in functions {
            // Unsupported signatures remain unavailable at the source boundary; unrelated
            // imports such as WASI resources must not prevent compilation of a world.
            let Some(params) = function
                .params
                .iter()
                .map(|p| {
                    Some((
                        p.name.clone(),
                        HirType::Generic {
                            base: "WitInput".into(),
                            type_args: vec![wit_types::ty(resolve, &p.ty)?],
                        },
                        false,
                    ))
                })
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let result = match function.result {
                Some(ty) => match wit_types::ty(resolve, &ty) {
                    Some(ty) => ty,
                    None => continue,
                },
                None => HirType::Void,
            };
            let Ok(abi_params) = function
                .params
                .iter()
                .map(|p| abi_type(resolve, &p.ty))
                .collect()
            else {
                continue;
            };
            let Ok(abi_result) = function
                .result
                .as_ref()
                .map(|ty| abi_type(resolve, ty))
                .transpose()
            else {
                continue;
            };
            let binding = format!("{module}#{}", to_camel_case(&function.name));
            foreign.insert(
                binding.clone(),
                Foreign::Function(FunctionType {
                    params,
                    return_type: Box::new(result),
                    is_async: function.kind.is_async(),
                    is_generator: false,
                }),
            );
            imports.push(Import {
                binding,
                module: module.clone(),
                name: function.name.clone(),
                params: abi_params,
                result: abi_result,
                asynchronous: function.kind.is_async(),
                kind: perry_codegen::boundary::ImportKind::Function,
            });
        }
    }
    imports
}
