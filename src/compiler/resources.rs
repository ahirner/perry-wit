//! Canonical resource identities and the SDK's explicit ownership helpers.
use perry_codegen::boundary::{Import, ImportKind, ResourceDefinition, ResourceOperation};
use perry_contract::names::{canonical_type, to_camel_case, to_pascal_case};
use perry_frontend::wit::WorldTypes;
use perry_wit_diagnostics::{Result, ensure_diag};
use std::collections::BTreeMap;
use wit_parser::{Resolve, Type, TypeDefKind, WorldId, WorldItem};

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
                        name: name.clone(),
                        exported,
                    },
                );
                let suffix = to_pascal_case(name);
                let resource = Type::Id(*id);
                let drop = (
                    format!("drop{suffix}"),
                    ResourceOperation::Drop,
                    resource,
                    None,
                );
                let exports = exported.then(|| {
                    [
                        (
                            format!("new{suffix}"),
                            ResourceOperation::New,
                            Type::F64,
                            Some(resource),
                        ),
                        (
                            format!("{}Rep", to_camel_case(name)),
                            ResourceOperation::Representation,
                            resource,
                            Some(Type::F64),
                        ),
                    ]
                });
                for (name, operation, param, result) in
                    std::iter::once(drop).chain(exports.into_iter().flatten())
                {
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
