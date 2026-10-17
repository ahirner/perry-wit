//! Canonical resource identities and the SDK's explicit ownership helpers.
use crate::sdk::{qualified_type_name, to_camel_case, to_pascal_case};
use anyhow::{Result, ensure};
use perry_codegen::boundary::{Import, ImportKind, ResourceDefinition, ResourceOperation};
use perry_lower::hir::{Foreign, FunctionType, Type as HirType};
use std::collections::{BTreeMap, HashMap};
use wit_parser::{Resolve, Type, TypeDefKind, TypeId, WorldId, WorldItem};

pub(super) fn canonical(resolve: &Resolve, mut id: TypeId) -> TypeId {
    while let TypeDefKind::Type(Type::Id(next)) = resolve.types[id].kind {
        id = next;
    }
    id
}

pub(super) fn source_type(resolve: &Resolve, id: TypeId) -> HirType {
    HirType::Named(qualified_type_name(resolve, canonical(resolve, id)))
}

pub(super) fn definitions(
    resolve: &Resolve,
    world: WorldId,
    foreign: &mut HashMap<String, Foreign>,
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
                let key = canonical(resolve, *id).index() as u32;
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
                let resource = source_type(resolve, *id);
                let suffix = to_pascal_case(name);
                let mut helpers = vec![(
                    format!("drop{suffix}"),
                    ResourceOperation::Drop,
                    resource.clone(),
                    HirType::Void,
                )];
                if exported {
                    helpers.extend([
                        (
                            format!("new{suffix}"),
                            ResourceOperation::New,
                            HirType::Number,
                            resource.clone(),
                        ),
                        (
                            format!("{}Rep", to_camel_case(name)),
                            ResourceOperation::Representation,
                            resource.clone(),
                            HirType::Number,
                        ),
                    ]);
                }
                for (name, operation, input, output) in helpers {
                    let binding = format!("{module}#{name}");
                    foreign.insert(
                        binding.clone(),
                        Foreign::Function(FunctionType {
                            params: vec![("value".into(), input, false)],
                            return_type: Box::new(output),
                            is_async: false,
                            is_generator: false,
                        }),
                    );
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
