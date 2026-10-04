use wit_parser::{Function, Resolve, World, WorldItem, WorldKey};

use crate::sdk::codegen::to_camel_case;

pub fn validate_implementation_names(resolve: &Resolve, world: &World) -> anyhow::Result<()> {
    let mut names = std::collections::BTreeMap::new();
    for (key, item) in &world.exports {
        let functions = match item {
            WorldItem::Function(function) => {
                vec![(to_camel_case(&function.name), function.name.clone())]
            }
            WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                .functions
                .values()
                .map(|function| {
                    (
                        interface_implementation_name(resolve, world, key, function),
                        core_export_name(resolve, key, function),
                    )
                })
                .collect(),
            WorldItem::Type { .. } => continue,
        };
        for (name, export) in functions {
            if let Some(previous) = names.insert(name.clone(), export.clone()) {
                anyhow::bail!(
                    "WIT exports '{previous}' and '{export}' both require TypeScript implementation '{name}'"
                );
            }
        }
    }
    Ok(())
}

pub fn core_export_name(resolve: &Resolve, key: &WorldKey, function: &Function) -> String {
    format!("{}#{}", resolve.name_world_key(key), function.name)
}

fn interface_member_name(resolve: &Resolve, key: &WorldKey, function: &Function) -> String {
    let interface = match key {
        WorldKey::Name(name) => name.as_str(),
        WorldKey::Interface(id) => resolve.interfaces[*id]
            .name
            .as_deref()
            .unwrap_or("interface"),
    };
    to_camel_case(&format!("{interface}-{}", function.name))
}

/// Uses an interface prefix for implementation functions, qualifying package collisions.
pub fn interface_implementation_name(
    resolve: &Resolve,
    world: &World,
    key: &WorldKey,
    function: &Function,
) -> String {
    let candidate = interface_member_name(resolve, key, function);
    let occurrences = world
        .exports
        .iter()
        .map(|(key, item)| match item {
            WorldItem::Function(function) => {
                usize::from(to_camel_case(&function.name) == candidate)
            }
            WorldItem::Interface { id, .. } => resolve.interfaces[*id]
                .functions
                .values()
                .filter(|function| interface_member_name(resolve, key, function) == candidate)
                .count(),
            _ => 0,
        })
        .sum::<usize>();
    if occurrences == 1 {
        candidate
    } else {
        to_camel_case(&core_export_name(resolve, key, function))
    }
}
