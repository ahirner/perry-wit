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

pub fn result_type(
    resolve: &wit_parser::Resolve,
    mut ty: wit_parser::Type,
) -> Option<&wit_parser::Result_> {
    while let wit_parser::Type::Id(id) = ty {
        match &resolve.types[id].kind {
            wit_parser::TypeDefKind::Type(inner) => ty = *inner,
            wit_parser::TypeDefKind::Result(result) => return Some(result),
            _ => break,
        }
    }
    None
}

pub mod resolution;
pub mod sdk;
pub use perry_source::{Module, Program, Source};
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic};
#[derive(Clone, Debug)]
pub enum Package {
    Sources(Vec<Source>),
    Encoded(Vec<u8>),
}

impl From<&[u8]> for Package {
    fn from(contents: &[u8]) -> Self {
        Self::Sources(vec![contents.into()])
    }
}

#[derive(Clone, Debug)]
pub struct Wit {
    pub application: Package,
    pub dependencies: Vec<Package>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artifact {
    pub name: String,
    pub contents: Vec<u8>,
}

mod generate;
pub use generate::{generate_sources, generate_wit};

impl From<Vec<Artifact>> for Wit {
    fn from(artifacts: Vec<Artifact>) -> Self {
        Self {
            application: Package::Sources(
                artifacts
                    .into_iter()
                    .map(|artifact| Source {
                        name: Some(artifact.name),
                        contents: artifact.contents,
                    })
                    .collect(),
            ),
            dependencies: vec![],
        }
    }
}
