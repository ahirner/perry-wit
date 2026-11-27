//! Canonical resource identities and the SDK's explicit ownership helpers.
use perry_codegen::boundary::{Import, ImportKind, ResourceDefinition, ResourceOperation};
use perry_contract::interface_resources;
use perry_frontend::wit::WorldTypes;
use perry_wit_diagnostics::{Result, ensure_diag};
use std::collections::BTreeMap;
use wit_parser::{Resolve, WorldId};

pub(super) fn definitions<'r>(
    resolve: &'r Resolve,
    world: WorldId,
    types: &mut WorldTypes<'r>,
    bindings: &mut Vec<Import>,
) -> Result<Vec<ResourceDefinition>> {
    let mut definitions = BTreeMap::new();
    for (exported, interfaces) in [
        (false, &resolve.worlds[world].imports),
        (true, &resolve.worlds[world].exports),
    ] {
        for (key, id) in perry_contract::world_interfaces(interfaces) {
            let module = resolve.name_world_key(key);
            for (name, id) in interface_resources(resolve, id) {
                let key = id.index() as u32;
                ensure_diag!(
                    !definitions.contains_key(&key),
                    crate::codes::CONTRACT_MISMATCH,
                    "resource {module}#{name} is declared on both sides of this world; import and export resource identities are distinct, so use an exported interface that aliases the imported resource or separate resource interfaces"
                );
                definitions.insert(
                    key,
                    ResourceDefinition {
                        key,
                        module: module.clone(),
                        name: name.to_owned(),
                        exported,
                    },
                );
                for &operation in ResourceOperation::for_interface(exported) {
                    let (_, param, result) = operation.signature(id);
                    let name = operation.name(name);
                    let binding =
                        types.import_function(resolve, &module, &name, [param], result, false);
                    bindings.retain(|old| old.binding != binding);
                    bindings.push(Import {
                        binding,
                        module: module.clone(),
                        name,
                        params: vec![],
                        indirect_size: None,
                        result: None,
                        asynchronous: false,
                        kind: ImportKind::Resource { key, operation },
                    });
                }
            }
        }
    }
    Ok(definitions.into_values().collect())
}
