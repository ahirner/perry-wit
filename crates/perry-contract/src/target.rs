//! Select the execution ABI from the selected world, never from unrelated WIT packages.
use std::collections::HashSet;
use wit_parser::{Resolve, Type, TypeDefKind, TypeId, World, WorldId, WorldItem};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    P2,
    P3,
}

pub fn for_world(resolve: &Resolve, world: WorldId) -> Target {
    for_definition(resolve, &resolve.worlds[world])
}

pub fn for_definition(resolve: &Resolve, world: &World) -> Target {
    let mut p3 = false;
    let mut visited = HashSet::new();
    for item in world.imports.values().chain(world.exports.values()) {
        if let WorldItem::Type { id, .. } = item {
            p3 |= async_type(resolve, *id, &mut visited);
        }
        if let WorldItem::Interface { id, .. } = item {
            for ty in resolve.interfaces[*id].types.values() {
                p3 |= async_type(resolve, *ty, &mut visited);
            }
            if let Some(package) = resolve.interfaces[*id].package {
                let name = &resolve.packages[package].name;
                if name.namespace == "wasi"
                    && let Some(version) = &name.version
                {
                    p3 |= version.major == 0 && version.minor == 3;
                }
            }
        }
    }
    for entry in crate::world_functions(resolve, world.imports.iter().chain(world.exports.iter())) {
        p3 |= entry.function.kind.is_async()
            || !entry.function.find_futures_and_streams(resolve).is_empty();
    }
    // A world may mix both generations: any native async construct selects the P3 ABI for
    // the component, where WASI 0.2 interfaces are ordinary synchronous imports and exports.
    if p3 { Target::P3 } else { Target::P2 }
}

fn async_type(resolve: &Resolve, id: TypeId, visited: &mut HashSet<TypeId>) -> bool {
    if !visited.insert(id) {
        return false;
    }
    let kind = &resolve.types[id].kind;
    let mut found = matches!(kind, TypeDefKind::Future(_) | TypeDefKind::Stream(_));
    crate::names::visit_type_children(kind, |ty| {
        if let Type::Id(id) = ty {
            found |= async_type(resolve, *id, visited);
        }
    });
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(source: &str) -> Target {
        let mut resolve = Resolve::default();
        let package = resolve.push_str("test.wit", source).unwrap();
        let world = resolve.select_world(&[package], Some("test")).unwrap();
        for_world(&resolve, world)
    }

    #[test]
    fn only_the_selected_world_selects_the_abi() {
        assert_eq!(
            target(
                "package test:target; world test { export run: func(); } world other { export run: async func(); }"
            ),
            Target::P2
        );
        assert_eq!(
            target("package test:target; world test { export run: async func(); }"),
            Target::P3
        );
        assert_eq!(
            target(
                "package test:target; interface values { type bytes = stream<u8>; } world test { export values; }"
            ),
            Target::P3
        );
    }

    #[test]
    fn native_async_selects_p3_for_a_world_that_also_uses_wasi_p2() {
        let wasi = "package wasi:demo@0.2.0; interface values { get: func() -> u32; }";
        let world = "package test:target; world test { import wasi:demo/values@0.2.0; export run: async func(); }";
        let mut resolve = Resolve::default();
        resolve.push_str("wasi.wit", wasi).unwrap();
        let package = resolve.push_str("test.wit", world).unwrap();
        let world = resolve.select_world(&[package], Some("test")).unwrap();
        assert_eq!(for_world(&resolve, world), Target::P3);
    }
}
