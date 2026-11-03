//! Standard `PackageMetadata` payloads scoped by `package-docs:<WIT package>` sections.
//! Package scoping allows a component to retain documentation from its dependencies.

use std::collections::{BTreeMap, BTreeSet};

use wasm_encoder::{Encode, Section};
use wit_parser::{PackageId, PackageMetadata, Resolve, World, WorldId, decoding::DecodedWasm};

pub(crate) const PREFIX: &str = "package-docs:";

pub(crate) fn decode(bytes: &[u8]) -> anyhow::Result<DecodedWasm> {
    let mut decoded = wit_parser::decoding::decode(bytes)?;
    let (resolve, root) = match &mut decoded {
        DecodedWasm::WitPackage(resolve, package) => (resolve, *package),
        DecodedWasm::Component(resolve, world) => {
            let package = resolve.worlds[*world]
                .package
                .ok_or_else(|| anyhow::anyhow!("component has no package"))?;
            (resolve, package)
        }
    };
    let packages = resolve
        .packages
        .iter()
        .map(|(id, package)| (package.name.to_string(), id))
        .collect::<BTreeMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut depth = 0;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        match payload? {
            wasmparser::Payload::Version { .. } => depth += 1,
            wasmparser::Payload::End(_) => depth -= 1,
            wasmparser::Payload::CustomSection(section) if depth == 1 => {
                if section.name() == PackageMetadata::SECTION_NAME {
                    if !seen.insert(root) {
                        return Err(anyhow::anyhow!("duplicate root package metadata"));
                    }
                    continue;
                }
                let Some(name) = section.name().strip_prefix(PREFIX) else {
                    continue;
                };
                let package = packages
                    .get(name)
                    .ok_or_else(|| anyhow::anyhow!("unknown package {name}"))?;
                if !seen.insert(*package) {
                    return Err(anyhow::anyhow!("duplicate package {name}"));
                }
                PackageMetadata::decode(section.data())
                    .and_then(|metadata| metadata.inject(resolve, *package))?;
            }
            _ => {}
        }
    }
    Ok(decoded)
}

/// Preserve metadata for the declarations that survived encoding.
pub(crate) fn append(
    bytes: &mut Vec<u8>,
    source: &Resolve,
    source_world: Option<WorldId>,
) -> anyhow::Result<()> {
    let decoded = wit_parser::decoding::decode(bytes)?;
    let (target, root, component_world) = match &decoded {
        DecodedWasm::WitPackage(resolve, package) => (resolve, *package, None),
        DecodedWasm::Component(resolve, world) => (
            resolve,
            resolve.worlds[*world]
                .package
                .ok_or_else(|| anyhow::anyhow!("component has no package"))?,
            Some(*world),
        ),
    };
    let interfaces = target
        .interfaces
        .iter()
        .filter_map(|(id, _)| target.id_of(id).map(|name| (name, id)))
        .collect::<BTreeMap<_, _>>();
    let mut projected = source.clone();
    for (id, interface) in &mut projected.interfaces {
        if let Some(target_id) = source.id_of(id).and_then(|name| interfaces.get(&name)) {
            let actual = &target.interfaces[*target_id];
            interface
                .functions
                .retain(|name, _| actual.functions.contains_key(name));
            interface
                .types
                .retain(|name, _| actual.types.contains_key(name));
        }
    }
    let mut packages = Vec::new();
    for (id, package) in &mut projected.packages {
        let Some(target_id) = target.package_names.get(&package.name) else {
            continue;
        };
        let actual = &target.packages[*target_id];
        package
            .interfaces
            .retain(|name, _| actual.interfaces.contains_key(name));
        package
            .worlds
            .retain(|name, _| actual.worlds.contains_key(name));
        for (name, world) in &package.worlds {
            retain_world(
                &mut projected.worlds[*world],
                source,
                &target.worlds[actual.worlds[name]],
                target,
            );
        }
        if *target_id == root {
            continue;
        }
        packages.push((id, actual.name.to_string()));
    }
    packages.sort_by(|left, right| left.1.cmp(&right.1));
    for (id, name) in packages {
        append_package(bytes, &projected, id, &name)?;
    }
    if let Some(actual) = component_world {
        let original = source_world
            .ok_or_else(|| anyhow::anyhow!("component metadata requires its source world"))?;
        let package = source.worlds[original]
            .package
            .ok_or_else(|| anyhow::anyhow!("source world has no package"))?;
        let name = target.worlds[actual].name.clone();
        if projected.packages[package].name != target.packages[root].name {
            projected.packages[package].interfaces.clear();
        }
        projected.packages[package].worlds = [(name.clone(), original)].into_iter().collect();
        projected.worlds[original].name = name;
        retain_world(
            &mut projected.worlds[original],
            source,
            &target.worlds[actual],
            target,
        );
        append_package(
            bytes,
            &projected,
            package,
            &target.packages[root].name.to_string(),
        )?;
    }
    Ok(())
}

fn retain_world(world: &mut World, source: &Resolve, actual: &World, target: &Resolve) {
    let imports = actual
        .imports
        .keys()
        .map(|key| target.name_world_key(key))
        .collect::<BTreeSet<_>>();
    let exports = actual
        .exports
        .keys()
        .map(|key| target.name_world_key(key))
        .collect::<BTreeSet<_>>();
    world
        .imports
        .retain(|key, _| imports.contains(&source.name_world_key(key)));
    world
        .exports
        .retain(|key, _| exports.contains(&source.name_world_key(key)));
}

fn append_package(
    bytes: &mut Vec<u8>,
    resolve: &Resolve,
    package: PackageId,
    name: &str,
) -> anyhow::Result<()> {
    let metadata = PackageMetadata::extract(resolve, package);
    if serde_json::to_value(&metadata)?
        .as_object()
        .is_some_and(|object| object.is_empty())
    {
        return Ok(());
    }
    let section = wasm_encoder::CustomSection {
        name: format!("{PREFIX}{name}").into(),
        data: metadata.encode()?.into(),
    };
    bytes.push(section.id());
    section.encode(bytes);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_parser::{ManglingAndAbi, TypeDefKind};

    #[test]
    fn compiler_preserves_scoped_package_metadata_through_stripping() -> anyhow::Result<()> {
        let source = r#"/// Application package.
            package demo:application@1.0.0;
            /// Public entry world.
            world component {
                export demo:provider/api@1.0.0;
                export demo:provider/api@2.0.0;
            }
            /// First provider package.
            package demo:provider@1.0.0 {
                interface api {
                    flags permissions {
                        /// First read permission.
                        read,
                    }
                    /// First operation.
                    run: func() -> u32;
                }
            }
            /// Second provider package.
            package demo:provider@2.0.0 {
                interface api {
                    flags permissions {
                        /// Second read permission.
                        read,
                    }
                    /// Second operation.
                    run: func() -> u32;
                }
            }
        "#;
        let mut resolve = Resolve::new();
        let package = resolve.push_str("documented.wit", source)?;
        let mut encoded = wit_component::encode(&resolve, package, false)?;
        append(&mut encoded, &resolve, None)?;
        let (resolve, package) = super::super::wit::resolve_contents(&crate::api::Wit {
            application: crate::api::Package::Encoded(encoded),
            dependencies: Vec::new(),
        })?;
        let world = resolve.packages[package].worlds["component"];
        let core = wit_component::dummy_module(&resolve, world, ManglingAndAbi::Standard32);
        let component = super::super::encode_resolved(&core, &resolve, world)?;
        let stripped = crate::strip::component(&component)?;
        let DecodedWasm::Component(decoded, root) = decode(&stripped)? else {
            unreachable!()
        };
        assert_eq!(
            decoded.worlds[root].docs.contents.as_deref(),
            Some("Public entry world.")
        );
        for (version, expected) in [("1.0.0", "First"), ("2.0.0", "Second")] {
            let name = format!("demo:provider@{version}");
            let (_, package) = decoded
                .packages
                .iter()
                .find(|(_, package)| package.name.to_string() == name)
                .unwrap();
            assert_eq!(
                package.docs.contents.as_deref(),
                Some(format!("{expected} provider package.").as_str())
            );
            let api = &decoded.interfaces[package.interfaces["api"]];
            assert_eq!(
                api.functions["run"].docs.contents.as_deref(),
                Some(format!("{expected} operation.").as_str())
            );
            let TypeDefKind::Flags(flags) = &decoded.types[api.types["permissions"]].kind else {
                unreachable!()
            };
            assert_eq!(
                flags.flags[0].docs.contents.as_deref(),
                Some(format!("{expected} read permission.").as_str())
            );
        }
        let mut duplicate = stripped.clone();
        let mut invalid = stripped.clone();
        for payload in wasmparser::Parser::new(0).parse_all(&stripped) {
            if let wasmparser::Payload::CustomSection(section) = payload?
                && section.name().starts_with(PREFIX)
            {
                invalid[usize::try_from(section.data_offset())?] = 255;
                duplicate.push(0);
                wasm_encoder::CustomSection {
                    name: section.name().into(),
                    data: section.data().into(),
                }
                .encode(&mut duplicate);
                break;
            }
        }
        assert!(
            decode(&duplicate)
                .err()
                .unwrap()
                .to_string()
                .contains("duplicate")
        );
        assert!(
            decode(&invalid)
                .err()
                .unwrap()
                .to_string()
                .contains("version")
        );
        let mut outer = wasm_encoder::Component::new();
        outer.section(&wasm_encoder::RawSection {
            id: 4,
            data: &stripped,
        });
        let outer = outer.finish();
        let DecodedWasm::Component(_, _) = decode(&outer)? else {
            unreachable!()
        };
        Ok(())
    }
}
