//! Canonical resource identities and the SDK's explicit ownership helpers.
use anyhow::{Result, ensure};
use perry_codegen::boundary::{Import, ImportKind, ResourceDefinition, ResourceOperation};
use perry_contract::names::canonical_type;
use perry_frontend::wit::{ResourceOperation as SourceResourceOperation, WorldTypes};
use std::collections::BTreeMap;
use wit_parser::{Resolve, TypeDefKind, WorldId, WorldItem};

pub(super) fn definitions(
    resolve: &Resolve,
    world: WorldId,
    types: &mut WorldTypes,
    bindings: &mut Vec<Import>,
) -> Result<Vec<ResourceDefinition>> {
    let mut definitions = BTreeMap::new();
    for (exported, interfaces) in [
        (false, &resolve.worlds[world].imports),
        (true, &resolve.worlds[world].exports),
    ] {
        for (key, item) in interfaces {
            let WorldItem::Interface { id, .. } = item else {
                continue;
            };
            let module = resolve.name_world_key(key);
            for (name, id) in &resolve.interfaces[*id].types {
                if !matches!(resolve.types[*id].kind, TypeDefKind::Resource) {
                    continue;
                }
                let key = canonical_type(resolve, *id).index() as u32;
                ensure!(
                    !definitions.contains_key(&key),
                    "resource {module}#{name} is declared on both sides of this world; import and export resource identities are distinct, so use an exported interface that aliases the imported resource or separate resource interfaces"
                );
                definitions.insert(
                    key,
                    ResourceDefinition {
                        key,
                        module: module.clone(),
                        name: name.clone(),
                        exported,
                    },
                );
                for helper in types.resource_helpers(resolve, *id, &module, name, exported)? {
                    let operation = match helper.operation {
                        SourceResourceOperation::Drop => ResourceOperation::Drop,
                        SourceResourceOperation::New => ResourceOperation::New,
                        SourceResourceOperation::Representation => {
                            ResourceOperation::Representation
                        }
                    };
                    let binding = helper.binding;
                    let name = helper.name;
                    bindings.retain(|old| old.binding != binding);
                    bindings.push(Import {
                        binding,
                        module: module.clone(),
                        name,
                        params: vec![],
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
