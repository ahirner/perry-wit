//! Versioned WASI and custom WIT package resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use wit_parser::{PackageId, Resolve, SourceMap, UnresolvedPackageGroup};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/wasi_packages.rs"));
}

fn source_package(sources: &[crate::api::Source]) -> Result<UnresolvedPackageGroup> {
    ensure!(!sources.is_empty(), "WIT package has no sources");
    let mut map = SourceMap::new();
    for (index, source) in sources.iter().enumerate() {
        map.push_str(
            source
                .name
                .as_deref()
                .unwrap_or(&format!("source-{index}.wit")),
            crate::api::text(&source.contents, source.name.as_deref())?,
        );
    }
    map.parse()
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))
}

/// Resolve only supplied package contents and the compiler's pinned standard packages.
pub(crate) fn resolve_contents(input: &crate::api::Wit) -> Result<(Resolve, PackageId)> {
    let mut resolve = Resolve::new();
    let mut groups = Vec::new();
    let mut application = None;
    for (index, package) in std::iter::once(&input.application)
        .chain(&input.dependencies)
        .enumerate()
    {
        match package {
            crate::api::Package::Sources(sources) => groups.push(source_package(sources)?),
            crate::api::Package::Encoded(bytes) => {
                let wit_parser::decoding::DecodedWasm::WitPackage(decoded, package) =
                    wit_parser::decoding::decode(bytes)?
                else {
                    anyhow::bail!("expected an encoded WIT package, not an application component");
                };
                let remap = resolve.merge(decoded)?;
                if index == 0 {
                    application = Some(remap.packages[package.index()]);
                }
            }
        }
    }
    let main = if application.is_none() {
        groups.remove(0)
    } else {
        // push_groups needs a main package; the marker only anchors explicit dependencies.
        UnresolvedPackageGroup::parse("dependencies.wit", "package perry:compiler-dependencies;")
            .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?
    };
    let package = resolve_with(main, &mut resolve, groups, false)?;
    Ok((resolve, application.unwrap_or(package)))
}

pub(crate) fn resolve_embedded_source(name: &str, source: &str) -> Result<(Resolve, PackageId)> {
    resolve_contents(&crate::api::Wit {
        application: crate::api::Package::Sources(vec![crate::api::Source {
            name: Some(name.into()),
            contents: source.as_bytes().to_vec(),
        }]),
        dependencies: Vec::new(),
    })
}

fn read_dependencies(dir: &Path, resolve: &mut Resolve) -> Result<Vec<UnresolvedPackageGroup>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = std::fs::read_dir(dir)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut groups = Vec::new();
    for entry in entries {
        let path = entry.path();
        let group = if path.is_dir() {
            UnresolvedPackageGroup::parse_dir(&path)?
        } else {
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("wit") => {
                    let source = std::fs::read_to_string(&path)?;
                    UnresolvedPackageGroup::parse(&path, &source)
                        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?
                }
                Some("wasm" | "wat") => {
                    resolve.push_file(&path)?;
                    continue;
                }
                _ => continue,
            }
        };
        groups.push(group);
    }
    Ok(groups)
}

pub fn resolve_wit(wit_dir: &Path) -> Result<(Resolve, PackageId)> {
    if !wit_dir.exists() {
        return Err(crate::PerryDiagnostic::error(
            crate::DiagnosticCode::WitResolution,
            format!("WIT directory '{}' does not exist", wit_dir.display()),
        )
        .with_help(format!(
            "Create '{}/world.wit' or pass '--wit <PATH>' to specify your WIT directory.",
            wit_dir.display()
        ))
        .into());
    }
    let mut resolve = Resolve::new();
    let main = UnresolvedPackageGroup::parse_dir(wit_dir).map_err(|error| {
        let mut diag = crate::PerryDiagnostic::error(
            crate::DiagnosticCode::WitResolution,
            format!("loading WIT package from {}", wit_dir.display()),
        );
        if error
            .chain()
            .any(|c| c.to_string().contains("no `package` header was found"))
        {
            diag = diag.with_help(
                "Add a package header such as 'package local:app;' at the top of your .wit file, or ensure the directory contains at least one .wit file.",
            );
        }
        error.context(diag)
    })?;
    let deps = read_dependencies(&wit_dir.join("deps"), &mut resolve)?;
    resolve_with(main, &mut resolve, deps, true)
        .with_context(|| format!("resolving WIT dependencies for {}", wit_dir.display()))
        .map(|package| (resolve, package))
}

pub(crate) fn select_world(
    resolve: &Resolve,
    package: PackageId,
    world: Option<&str>,
) -> Result<wit_parser::WorldId> {
    resolve.select_world(&[package], world).map_err(|error| {
        let worlds = &resolve.packages[package].worlds;
        let available = worlds
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let help = match (worlds.is_empty(), world.is_some()) {
            (true, _) => "Declare a 'world <name> { ... }' block in your WIT package.".to_owned(),
            (false, true) => format!(
                "Available worlds in package '{}': {available}.",
                resolve.packages[package].name
            ),
            (false, false) => {
                format!("Pass '--world <NAME>' to select a world (available: {available}).")
            }
        };
        crate::PerryDiagnostic::error(crate::DiagnosticCode::WitResolution, error.to_string())
            .with_help(help)
            .into()
    })
}

/// Resolve a WIT package given as source, with the ambient WASI packages.
pub fn resolve_source(name: &str, source: &str) -> Result<(Resolve, PackageId)> {
    let mut resolve = Resolve::new();
    let main = UnresolvedPackageGroup::parse(name, source)
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
    resolve_with(main, &mut resolve, Vec::new(), true)
        .with_context(|| format!("resolving WIT dependencies for {name}"))
        .map(|package| (resolve, package))
}

fn resolve_with(
    main: UnresolvedPackageGroup,
    resolve: &mut Resolve,
    mut deps: Vec<UnresolvedPackageGroup>,
    ambient: bool,
) -> Result<PackageId> {
    let ambient_groups = match ambient.then(|| std::env::var_os("WASI_WIT_PATH")).flatten() {
        Some(path) => read_dependencies(&PathBuf::from(path), resolve)?,
        None => Vec::new(),
    };
    let mut available: HashSet<_> = resolve.package_names.keys().cloned().collect();
    for group in std::iter::once(&main).chain(deps.iter()) {
        available.insert(group.main.name.clone());
        available.extend(group.nested.iter().map(|package| package.name.clone()));
    }
    let mut push_unique = |mut group: UnresolvedPackageGroup| {
        if available.insert(group.main.name.clone()) {
            group
                .nested
                .retain(|package| available.insert(package.name.clone()));
            deps.push(group);
        }
    };
    for group in ambient_groups {
        push_unique(group);
    }
    for files in embedded::PACKAGES {
        let mut map = SourceMap::new();
        for (name, source) in *files {
            map.push_str(name, *source);
        }
        push_unique(
            map.parse()
                .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?,
        );
    }
    Ok(resolve.push_groups(main, deps)?)
}
