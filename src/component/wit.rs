//! Versioned WASI and custom WIT package resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use perry_wit_diagnostics::{Diagnostic, Result, internal};
use wit_parser::{PackageId, Resolve, SourceMap, UnresolvedPackageGroup};

fn wit_diagnostic(code: crate::Code, source: &(dyn std::error::Error + 'static)) -> Diagnostic {
    let mut diagnostic = Diagnostic::new(code, source.to_string());
    if let Some(cause) = source.source() {
        diagnostic = diagnostic.with_source_notes(cause);
    }
    diagnostic
}

fn has_io_error(mut err: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if err.downcast_ref::<std::io::Error>().is_some() {
            return true;
        }
        let Some(source) = err.source() else {
            return false;
        };
        err = source;
    }
}

fn wit_parse_error((map, error): (SourceMap, wit_parser::ParseError)) -> Diagnostic {
    Diagnostic::new(crate::codes::WIT_PARSE, error.render(&map))
}

fn wrap_wit(err: Diagnostic, label: String) -> Diagnostic {
    err.wrap_boundary(None, format!("while {label}"), label)
}

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/wasi_packages.rs"));
}

fn source_package(sources: &[crate::api::Source]) -> Result<UnresolvedPackageGroup> {
    if sources.is_empty() {
        return Err(Diagnostic::new(
            crate::codes::INVALID_INPUT,
            "WIT package has no sources",
        ));
    }
    let mut map = SourceMap::new();
    for (index, source) in sources.iter().enumerate() {
        let text = crate::api::text(&source.contents, source.name.as_deref())?;
        match source.name.as_deref() {
            Some(name) => map.push_str(name, text),
            None => map.push_str(&format!("source-{index}.wit"), text),
        }
    }
    map.parse().map_err(wit_parse_error)
}

/// Resolve only supplied package contents and the compiler's pinned standard packages.
pub(crate) fn resolve_contents(input: &crate::api::Wit) -> Result<(Resolve, PackageId)> {
    let mut resolve = Resolve::new();
    let mut decode_encoded = |bytes: &[u8]| -> Result<PackageId> {
        let decoded = wit_parser::decoding::decode(bytes)
            .map_err(|e| wit_diagnostic(crate::codes::WIT_PARSE, e.as_ref()))?;
        let wit_parser::decoding::DecodedWasm::WitPackage(decoded, package) = decoded else {
            return Err(Diagnostic::new(
                crate::codes::WIT_PARSE,
                "expected an encoded WIT package, not an application component",
            ));
        };
        let remap = resolve
            .merge(decoded)
            .map_err(|e| wit_diagnostic(crate::codes::WIT_DEPENDENCY, e.as_ref()))?;
        Ok(remap.packages[package.index()])
    };
    let (main, application) = match &input.application {
        crate::api::Package::Sources(sources) => (source_package(sources)?, None),
        crate::api::Package::Encoded(bytes) => {
            let package = decode_encoded(bytes)?;
            // push_groups needs a main package; the marker only anchors explicit dependencies.
            let marker = UnresolvedPackageGroup::parse(
                "dependencies.wit",
                "package perry:compiler-dependencies;",
            )
            .map_err(|(map, error)| {
                internal!("parsing dependency anchor WIT: {}", error.render(&map))
            })?;
            (marker, Some(package))
        }
    };
    let mut groups = Vec::new();
    for package in &input.dependencies {
        match package {
            crate::api::Package::Sources(sources) => groups.push(source_package(sources)?),
            crate::api::Package::Encoded(bytes) => {
                decode_encoded(bytes)?;
            }
        }
    }
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

fn parse_dir(path: &Path) -> Result<UnresolvedPackageGroup> {
    let mut map = SourceMap::default();
    map.push_dir(path).map_err(|err| {
        let err: &(dyn std::error::Error + 'static) = err.as_ref();
        if has_io_error(err) {
            Diagnostic::io(format!("reading WIT directory {}", path.display()), err)
        } else {
            wit_diagnostic(crate::codes::WIT_PARSE, err)
        }
    })?;
    map.parse().map_err(|err| {
        let diagnostic = wit_parse_error(err);
        if diagnostic.message().contains("no `package` header was found") {
            diagnostic.with_help(
                "Add a package header such as 'package local:app;' at the top of your .wit file, or ensure the directory contains at least one .wit file.",
            )
        } else {
            diagnostic
        }
    })
}

fn read_dependencies(dir: &Path, resolve: &mut Resolve) -> Result<Vec<UnresolvedPackageGroup>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let io_dir = |err: std::io::Error| {
        Diagnostic::io(
            format!("reading WIT dependency directory {}", dir.display()),
            &err,
        )
    };
    let mut entries = std::fs::read_dir(dir)
        .map_err(io_dir)?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(io_dir)?;
    entries.sort_by_key(|entry| entry.file_name());
    let mut groups = Vec::new();
    for entry in entries {
        let path = entry.path();
        let group = if path.is_dir() {
            parse_dir(&path)?
        } else {
            match path.extension().and_then(|ext| ext.to_str()) {
                Some("wit") => {
                    let source = std::fs::read_to_string(&path).map_err(|err| {
                        Diagnostic::io(format!("reading WIT file {}", path.display()), &err)
                    })?;
                    UnresolvedPackageGroup::parse(&path, &source).map_err(wit_parse_error)?
                }
                Some("wasm" | "wat") => {
                    resolve.push_file(&path).map_err(|err| {
                        let err: &(dyn std::error::Error + 'static) = err.as_ref();
                        if has_io_error(err) {
                            Diagnostic::io(format!("reading WIT package {}", path.display()), err)
                        } else {
                            wit_diagnostic(crate::codes::WIT_PARSE, err)
                        }
                    })?;
                    continue;
                }
                _ => continue,
            }
        };
        groups.push(group);
    }
    Ok(groups)
}

pub fn resolve_wit(wit_dir: &Path) -> Result<(Resolve, PackageId), crate::CompileError> {
    if !wit_dir.exists() {
        return Err(Diagnostic::new(
            crate::codes::WIT_DEPENDENCY,
            format!("WIT directory '{}' does not exist", wit_dir.display()),
        )
        .with_help(format!(
            "Create '{}/world.wit' or pass '--wit <PATH>' to specify your WIT directory.",
            wit_dir.display()
        ))
        .into());
    }
    let mut resolve = Resolve::new();
    let main = parse_dir(wit_dir).map_err(|err| {
        wrap_wit(
            err,
            format!("loading WIT package from {}", wit_dir.display()),
        )
    })?;
    let deps = read_dependencies(&wit_dir.join("deps"), &mut resolve)?;
    let package = resolve_with(main, &mut resolve, deps, true).map_err(|err| {
        wrap_wit(
            err,
            format!("resolving WIT dependencies for {}", wit_dir.display()),
        )
    })?;
    Ok((resolve, package))
}

pub(crate) fn select_world(
    resolve: &Resolve,
    package: PackageId,
    world: Option<&str>,
) -> Result<wit_parser::WorldId> {
    resolve.select_world(&[package], world).map_err(|source| {
        let pkg = &resolve.packages[package];
        let available: Vec<&str> = pkg.worlds.keys().map(String::as_str).collect();
        let help = match (available.is_empty(), world.is_some()) {
            (true, _) => "Declare a 'world <name> { ... }' block in your WIT package.".to_owned(),
            (false, true) => format!(
                "Available worlds in package '{}': {}.",
                pkg.name,
                available.join(", ")
            ),
            (false, false) => format!(
                "Pass '--world <NAME>' to select a world (available: {}).",
                available.join(", ")
            ),
        };
        wit_diagnostic(crate::codes::WIT_WORLD_SELECTION, source.as_ref()).with_help(help)
    })
}

/// Resolve a WIT package given as source, with the ambient WASI packages.
pub fn resolve_source(
    name: &str,
    source: &str,
) -> Result<(Resolve, PackageId), crate::CompileError> {
    let mut resolve = Resolve::new();
    let main = UnresolvedPackageGroup::parse(name, source).map_err(wit_parse_error)?;
    let package = resolve_with(main, &mut resolve, Vec::new(), true)
        .map_err(|err| wrap_wit(err, format!("resolving WIT dependencies for {name}")))?;
    Ok((resolve, package))
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
    let mut required = Vec::new();
    for group in std::iter::once(&main).chain(deps.iter()) {
        for package in std::iter::once(&group.main).chain(group.nested.iter()) {
            available.insert(package.name.clone());
            required.extend(package.foreign_deps.keys().cloned());
        }
    }
    let mut candidates = ambient_groups;
    for files in embedded::PACKAGES {
        let mut map = SourceMap::new();
        for (name, source) in *files {
            map.push_str(name, *source);
        }
        candidates.push(map.parse().map_err(|(map, error)| {
            internal!("parsing embedded WIT package: {}", error.render(&map))
        })?);
    }
    let mut selected = HashSet::new();
    while let Some(name) = required.pop() {
        if available.contains(&name) {
            continue;
        }
        let Some(package) = candidates
            .iter()
            .flat_map(|group| std::iter::once(&group.main).chain(group.nested.iter()))
            .find(|package| package.name == name)
        else {
            continue;
        };
        required.extend(package.foreign_deps.keys().cloned());
        available.insert(name.clone());
        selected.insert(name);
    }
    for mut group in candidates {
        let include_main = selected.remove(&group.main.name);
        group
            .nested
            .retain(|package| selected.remove(&package.name));
        if !include_main {
            let Some(package) = group.nested.pop() else {
                continue;
            };
            group.main = package;
        }
        deps.push(group);
    }
    resolve
        .push_groups(main, deps)
        .map_err(|err| wit_diagnostic(crate::codes::WIT_DEPENDENCY, &err))
}
