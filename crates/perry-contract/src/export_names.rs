use super::world_functions;
use wit_parser::{Function, Resolve, World, WorldKey};

use crate::names::to_camel_case;

pub fn validate_implementation_names(
    resolve: &Resolve,
    world: &World,
) -> Result<(), ImplementationNameCollision> {
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
                return Err(ImplementationNameCollision {
                    first: entry.get().clone(),
                    second: export,
                    implementation: entry.key().clone(),
                });
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

#[derive(Debug)]
pub struct ImplementationNameCollision {
    first: String,
    second: String,
    implementation: String,
}

impl std::fmt::Display for ImplementationNameCollision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "WIT exports '{}' and '{}' both require TypeScript implementation '{}'",
            self.first, self.second, self.implementation,
        )
    }
}

impl std::error::Error for ImplementationNameCollision {}
