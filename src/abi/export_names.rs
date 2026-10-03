use wit_parser::{Function, InterfaceId, Resolve, World, WorldItem, WorldKey};

use crate::sdk::codegen::to_camel_case;

/// Identifies the WASI HTTP resource export served by the runtime's generated adapter.
pub(crate) fn is_incoming_handler(resolve: &Resolve, id: InterfaceId) -> bool {
    let interface = &resolve.interfaces[id];
    interface.name.as_deref() == Some("incoming-handler")
        && interface.package.is_some_and(|package| {
            let name = &resolve.packages[package].name;
            name.namespace == "wasi" && name.name == "http"
        })
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
