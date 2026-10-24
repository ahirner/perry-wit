//! Canonical bindings for the functions exposed by the frontend's WIT source view.
use super::bind::abi_type;
use perry_codegen::boundary::Import;
use perry_frontend::wit::WorldTypes;
use wit_parser::{Resolve, WorldId};

pub(super) fn bindings(resolve: &Resolve, world: WorldId, types: &mut WorldTypes) -> Vec<Import> {
    let mut imports = Vec::new();
    for entry in perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter()) {
        let module = if entry.interface.is_some() {
            resolve.name_world_key(entry.key)
        } else {
            "$root".into()
        };
        let function = entry.function;
        let Ok(params) = function
            .params
            .iter()
            .map(|parameter| abi_type(resolve, &parameter.ty))
            .collect()
        else {
            continue;
        };
        let Ok(result) = function
            .result
            .as_ref()
            .map(|ty| abi_type(resolve, ty))
            .transpose()
        else {
            continue;
        };
        let Some(binding) = types.import_function(resolve, &module, function) else {
            continue;
        };
        imports.push(Import {
            binding,
            module,
            name: function.name.clone(),
            params,
            result,
            asynchronous: function.kind.is_async(),
            kind: perry_codegen::boundary::ImportKind::Function,
        });
    }
    imports
}
