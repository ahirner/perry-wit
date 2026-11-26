//! Component contracts shared by the compiler and SDK.

pub mod export_names;
pub mod names;
pub mod types;

use wit_parser::{Function, InterfaceId, Resolve, World, WorldItem, WorldKey};

/// A function keeps its world key and optional interface owner when flattened.
pub struct WorldFunction<'a> {
    pub key: &'a WorldKey,
    pub function: &'a Function,
    pub interface: Option<InterfaceId>,
}

impl WorldFunction<'_> {
    pub fn is_command(&self, resolve: &Resolve) -> bool {
        let WorldKey::Interface(id) = self.key else {
            return false;
        };
        let interface = &resolve.interfaces[*id];
        self.function.name == "run"
            && interface.name.as_deref() == Some("run")
            && interface.package.is_some_and(|id| {
                let name = &resolve.packages[id].name;
                name.namespace == "wasi" && name.name == "cli"
            })
    }

    pub fn implementation_name(&self, resolve: &Resolve, world: &World) -> String {
        if self.interface.is_some() {
            export_names::interface_implementation_name(resolve, world, self.key, self.function)
        } else {
            crate::names::to_camel_case(&self.function.name)
        }
    }
}

/// Borrow direct and interface functions in the world's declaration order.
pub fn world_functions<'a>(
    resolve: &'a Resolve,
    items: impl Iterator<Item = (&'a WorldKey, &'a WorldItem)> + 'a,
) -> impl Iterator<Item = WorldFunction<'a>> + 'a {
    items.flat_map(move |(key, item)| {
        let (single, interface) = match item {
            WorldItem::Function(function) => (Some(function), None),
            WorldItem::Interface { id, .. } => (None, Some(*id)),
            WorldItem::Type { .. } => (None, None),
        };
        single
            .into_iter()
            .chain(
                interface
                    .into_iter()
                    .flat_map(move |id| resolve.interfaces[id].functions.values()),
            )
            .map(move |function| WorldFunction {
                key,
                function,
                interface,
            })
    })
}

pub fn type_kind(resolve: &Resolve, ty: wit_parser::Type) -> Option<&wit_parser::TypeDefKind> {
    let wit_parser::Type::Id(id) = ty else {
        return None;
    };
    Some(&resolve.types[names::canonical_type(resolve, id)].kind)
}

pub fn result_type(resolve: &Resolve, ty: wit_parser::Type) -> Option<&wit_parser::Result_> {
    match type_kind(resolve, ty)? {
        wit_parser::TypeDefKind::Result(result) => Some(result),
        _ => None,
    }
}
