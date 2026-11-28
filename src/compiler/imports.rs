//! Canonical bindings for the functions exposed by the frontend's WIT source view.
use super::abi::Layouts;
use perry_codegen::boundary::Import;
use perry_frontend::wit::WorldTypes;
use wit_parser::WorldId;

pub(super) fn bindings<'r>(
    layouts: &mut Layouts<'r>,
    world: WorldId,
) -> (WorldTypes<'r>, Vec<Import>) {
    WorldTypes::with_imports(layouts.resolve, Some(world), |module, binding, function| {
        let result = function
            .result
            .map(|ty| layouts.value(ty))
            .transpose()
            .ok()?;
        let core = layouts.signature(function, true);
        let (params, indirect_size) = layouts.parameters(function, &core).ok()?;
        Some(Import {
            binding: binding.into(),
            module: module.into(),
            name: function.name.clone(),
            params,
            indirect_size,
            result,
            asynchronous: function.kind.is_async(),
            kind: perry_codegen::boundary::ImportKind::Function(core),
        })
    })
}
