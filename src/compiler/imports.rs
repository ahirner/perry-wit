//! Canonical bindings for the functions exposed by the frontend's WIT source view.
use super::abi::Layouts;
use perry_codegen::boundary::Import;
use perry_frontend::wit::WorldTypes;
use wit_parser::{Resolve, WorldId};

pub(super) fn bindings<'r>(
    layouts: &Layouts,
    resolve: &'r Resolve,
    world: WorldId,
    types: &mut WorldTypes<'r>,
) -> Vec<Import> {
    let mut imports = Vec::new();
    for entry in perry_contract::world_functions(resolve, resolve.worlds[world].imports.iter()) {
        let module = if entry.interface.is_some() {
            resolve.name_world_key(entry.key)
        } else {
            "$root".into()
        };
        let function = entry.function;
        let Ok(result) = function
            .result
            .as_ref()
            .map(|ty| layouts.value(*ty))
            .transpose()
        else {
            continue;
        };
        let core = layouts.signature(function, true);
        let Ok((params, indirect_size)) = layouts.parameters(function, &core) else {
            continue;
        };
        let binding = types.import_function(
            resolve,
            &module,
            &perry_contract::names::to_camel_case(&function.name),
            function.params.iter().map(|p| p.ty),
            function.result,
            function.kind.is_async(),
        );
        imports.push(Import {
            binding,
            module,
            name: function.name.clone(),
            params,
            indirect_size,
            result,
            asynchronous: function.kind.is_async(),
            kind: perry_codegen::boundary::ImportKind::Function(core),
        });
    }
    imports
}
