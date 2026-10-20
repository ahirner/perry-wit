use super::world_functions;
use wit_parser::{Function, Resolve, World, WorldKey};

use crate::sdk::to_camel_case;

pub(crate) fn validate_implementation_names(
    resolve: &Resolve,
    world: &World,
) -> anyhow::Result<()> {
    let mut names = std::collections::BTreeMap::new();
    for entry in world_functions(resolve, world.exports.iter()) {
        let name = entry.implementation_name(resolve, world);
        let export = if entry.interface.is_some() {
            core_export_name(resolve, entry.key, entry.function)
        } else {
            entry.function.name.clone()
        };
        match names.entry(name) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(export);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                anyhow::bail!(
                    "WIT exports '{}' and '{export}' both require TypeScript implementation '{}'",
                    entry.get(),
                    entry.key(),
                );
            }
        }
    }
    Ok(())
}

pub(crate) fn core_export_name(resolve: &Resolve, key: &WorldKey, function: &Function) -> String {
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
pub(crate) fn interface_implementation_name(
    resolve: &Resolve,
    world: &World,
    key: &WorldKey,
    function: &Function,
) -> String {
    let candidate = interface_member_name(resolve, key, function);
    let occurrences = world_functions(resolve, world.exports.iter())
        .filter(|entry| {
            let name = if entry.interface.is_some() {
                interface_member_name(resolve, entry.key, entry.function)
            } else {
                to_camel_case(&entry.function.name)
            };
            name == candidate
        })
        .count();
    if occurrences == 1 {
        candidate
    } else {
        to_camel_case(&core_export_name(resolve, key, function))
    }
}
