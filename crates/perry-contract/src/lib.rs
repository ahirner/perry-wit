//! Component contracts shared by the compiler and SDK.

pub mod export_names;
pub mod names;
pub mod target;
pub mod types;

use wit_parser::{Function, InterfaceId, Resolve, World, WorldItem, WorldKey};

/// A function keeps its world key and optional interface owner when flattened.
#[derive(Clone, Copy)]
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

    pub fn core_name(&self, resolve: &Resolve) -> String {
        if self.interface.is_some() {
            export_names::core_export_name(resolve, self.key, self.function)
        } else {
            self.function.name.clone()
        }
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

/// Interfaces in world declaration order, including named inline interfaces.
pub fn world_interfaces<'a>(
    items: impl IntoIterator<Item = (&'a WorldKey, &'a WorldItem)>,
) -> impl Iterator<Item = (&'a WorldKey, InterfaceId)> {
    items.into_iter().filter_map(|(key, item)| match item {
        WorldItem::Interface { id, .. } => Some((key, *id)),
        _ => None,
    })
}

/// Resource declarations owned by an interface, in WIT declaration order.
pub fn interface_resources(
    resolve: &Resolve,
    id: InterfaceId,
) -> impl Iterator<Item = (&str, wit_parser::TypeId)> {
    resolve.interfaces[id]
        .types
        .iter()
        .filter_map(|(name, &id)| {
            matches!(resolve.types[id].kind, wit_parser::TypeDefKind::Resource)
                .then_some((name.as_str(), id))
        })
}

#[derive(Clone, Copy, Debug)]
pub enum ResourceOperation {
    Drop,
    New,
    Representation,
}

impl ResourceOperation {
    pub fn for_interface(exported: bool) -> &'static [Self] {
        if exported {
            &[Self::Drop, Self::New, Self::Representation]
        } else {
            &[Self::Drop]
        }
    }

    pub fn name(self, resource: &str) -> String {
        match self {
            Self::Drop => format!("drop{}", names::to_pascal_case(resource)),
            Self::New => format!("new{}", names::to_pascal_case(resource)),
            Self::Representation => format!("{}Rep", names::to_camel_case(resource)),
        }
    }

    pub fn signature(
        self,
        resource: wit_parser::TypeId,
    ) -> (&'static str, wit_parser::Type, Option<wit_parser::Type>) {
        use wit_parser::Type::{F64, Id};
        match self {
            Self::Drop => ("value", Id(resource), None),
            Self::New => ("representation", F64, Some(Id(resource))),
            Self::Representation => ("value", Id(resource), Some(F64)),
        }
    }
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
