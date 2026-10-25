//! Versioned WASI and custom WIT package resolution.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use wit_parser::{PackageId, Resolve, SourceMap, UnresolvedPackageGroup};

#[derive(Debug)]
enum WitError {
    MissingDirectory {
        dir: PathBuf,
    },
    ParsePackage {
        dir: PathBuf,
        missing_package_header: bool,
        source: anyhow::Error,
    },
    ResolveDependencies {
        target: String,
        source: anyhow::Error,
    },
    SelectWorld {
        package_name: String,
        has_requested_world: bool,
        available: Vec<String>,
        source: anyhow::Error,
    },
    EmptySources,
    InvalidPackageEncoding,
    Other(anyhow::Error),
}

impl WitError {
    fn help(&self) -> Option<String> {
        match self {
            Self::MissingDirectory { dir } => Some(format!(
                "Create '{}/world.wit' or pass '--wit <PATH>' to specify your WIT directory.",
                dir.display()
            )),
            Self::ParsePackage {
                missing_package_header: true,
                ..
            } => Some(
                "Add a package header such as 'package local:app;' at the top of your .wit file, or ensure the directory contains at least one .wit file.".to_owned(),
            ),
            Self::SelectWorld {
                package_name,
                has_requested_world,
                available,
                ..
            } => Some(match (available.is_empty(), *has_requested_world) {
                (true, _) => {
                    "Declare a 'world <name> { ... }' block in your WIT package.".to_owned()
                }
                (false, true) => {
                    format!(
                        "Available worlds in package '{package_name}': {}.",
                        available.join(", ")
                    )
                }
                (false, false) => {
                    format!(
                        "Pass '--world <NAME>' to select a world (available: {}).",
                        available.join(", ")
                    )
                }
            }),
            _ => None,
        }
    }
}

impl fmt::Display for WitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingDirectory { dir } => {
                write!(f, "WIT directory '{}' does not exist", dir.display())
            }
            Self::ParsePackage { dir, .. } => {
                write!(f, "loading WIT package from {}", dir.display())
            }
            Self::ResolveDependencies { target, .. } => {
                write!(f, "resolving WIT dependencies for {target}")
            }
            Self::SelectWorld { source, .. } | Self::Other(source) => write!(f, "{source}"),
            Self::EmptySources => f.write_str("WIT package has no sources"),
            Self::InvalidPackageEncoding => {
                f.write_str("expected an encoded WIT package, not an application component")
            }
        }
    }
}

impl std::error::Error for WitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ParsePackage { source, .. } | Self::ResolveDependencies { source, .. } => {
                Some(source.as_ref())
            }
            Self::SelectWorld { source, .. } | Self::Other(source) => source.source(),
            Self::MissingDirectory { .. } | Self::EmptySources | Self::InvalidPackageEncoding => {
                None
            }
        }
    }
}

impl From<WitError> for crate::PerryDiagnostic {
    fn from(error: WitError) -> Self {
        let help = error.help();
        let mut diagnostic = Self::error(crate::DiagnosticCode::WitResolution, error.to_string());
        if let Some(help) = help {
            diagnostic = diagnostic.with_help(help);
        }
        if let Some(source) = std::error::Error::source(&error) {
            diagnostic = diagnostic.with_source_notes(source);
        }
        diagnostic
    }
}

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/wasi_packages.rs"));
}

fn source_package(sources: &[crate::Source]) -> Result<UnresolvedPackageGroup> {
    if sources.is_empty() {
        return Err(crate::PerryDiagnostic::from(WitError::EmptySources).into());
    }
    let mut map = SourceMap::new();
    for (index, source) in sources.iter().enumerate() {
        let text = perry_source::text(&source.contents, source.name.as_deref())
            .map_err(|e| crate::PerryDiagnostic::from(WitError::Other(e)))?;
        match source.name.as_deref() {
            Some(name) => map.push_str(name, text),
            None => map.push_str(&format!("source-{index}.wit"), text),
        }
    }
    map.parse().map_err(|(map, error)| {
        crate::PerryDiagnostic::from(WitError::Other(anyhow::anyhow!(error.render(&map)))).into()
    })
}

/// Resolve only supplied package contents and the compiler's pinned standard packages.
pub fn resolve_contents(input: &crate::Wit) -> Result<(Resolve, PackageId)> {
    let mut resolve = Resolve::new();
    let mut groups = Vec::new();
    let mut application = None;
    for (index, package) in std::iter::once(&input.application)
        .chain(&input.dependencies)
        .enumerate()
    {
        match package {
            crate::Package::Sources(sources) => groups.push(source_package(sources)?),
            crate::Package::Encoded(bytes) => {
                let decoded = wit_parser::decoding::decode(bytes)
                    .map_err(|e| crate::PerryDiagnostic::from(WitError::Other(e)))?;
                let wit_parser::decoding::DecodedWasm::WitPackage(decoded, package) = decoded
                else {
                    return Err(
                        crate::PerryDiagnostic::from(WitError::InvalidPackageEncoding).into(),
                    );
                };
                let remap = resolve
                    .merge(decoded)
                    .map_err(|e| crate::PerryDiagnostic::from(WitError::Other(e)))?;
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
            .map_err(|(map, error)| {
                crate::PerryDiagnostic::from(WitError::Other(anyhow::anyhow!(error.render(&map))))
            })?
    };
    let package = resolve_with(main, &mut resolve, groups, false)
        .map_err(|e| crate::PerryDiagnostic::from(WitError::Other(e)))?;
    Ok((resolve, application.unwrap_or(package)))
}

pub fn resolve_embedded_source(name: &str, source: &str) -> Result<(Resolve, PackageId)> {
    resolve_contents(&crate::Wit {
        application: crate::Package::Sources(vec![crate::Source {
            name: Some(name.into()),
            contents: source.as_bytes().to_vec(),
        }]),
        dependencies: Vec::new(),
    })
}

fn parse_dir(path: &Path) -> Result<UnresolvedPackageGroup> {
    let mut map = SourceMap::default();
    map.push_dir(path)?;
    map.parse()
        .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))
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
            parse_dir(&path)?
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
        return Err(crate::PerryDiagnostic::from(WitError::MissingDirectory {
            dir: wit_dir.to_path_buf(),
        })
        .into());
    }
    let mut resolve = Resolve::new();
    let main = parse_dir(wit_dir).map_err(|source| {
        let missing_package_header = source
            .chain()
            .any(|c| c.to_string().contains("no `package` header was found"));
        crate::PerryDiagnostic::from(WitError::ParsePackage {
            dir: wit_dir.to_path_buf(),
            missing_package_header,
            source,
        })
    })?;
    let deps = read_dependencies(&wit_dir.join("deps"), &mut resolve)
        .map_err(|e| crate::PerryDiagnostic::from(WitError::Other(e)))?;
    resolve_with(main, &mut resolve, deps, true)
        .map_err(|source| {
            crate::PerryDiagnostic::from(WitError::ResolveDependencies {
                target: wit_dir.display().to_string(),
                source,
            })
            .into()
        })
        .map(|package| (resolve, package))
}

pub fn select_world(
    resolve: &Resolve,
    package: PackageId,
    world: Option<&str>,
) -> Result<wit_parser::WorldId> {
    resolve.select_world(&[package], world).map_err(|source| {
        let available = resolve.packages[package].worlds.keys().cloned().collect();
        crate::PerryDiagnostic::from(WitError::SelectWorld {
            package_name: resolve.packages[package].name.to_string(),
            has_requested_world: world.is_some(),
            available,
            source,
        })
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
