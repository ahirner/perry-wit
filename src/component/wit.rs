//! WASI Preview 2 and custom WIT package resolution.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use wit_parser::{PackageId, Resolve, UnresolvedPackageGroup};

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
    let mut resolve = Resolve::new();
    let main = UnresolvedPackageGroup::parse_dir(wit_dir)
        .with_context(|| format!("loading WIT package from {}", wit_dir.display()))?;
    let mut deps = read_dependencies(&wit_dir.join("deps"), &mut resolve)?;
    let mut available: HashSet<_> = resolve.package_names.keys().cloned().collect();
    for group in std::iter::once(&main).chain(deps.iter()) {
        available.insert(group.main.name.clone());
        available.extend(group.nested.iter().map(|package| package.name.clone()));
    }

    let ambient = std::env::var_os("WASI_WIT_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("wit/deps"));
    for mut group in read_dependencies(&ambient, &mut resolve)? {
        if available.insert(group.main.name.clone()) {
            group
                .nested
                .retain(|package| available.insert(package.name.clone()));
            deps.push(group);
        }
    }

    let pkg_id = resolve
        .push_groups(main, deps)
        .with_context(|| format!("resolving WIT dependencies for {}", wit_dir.display()))?;
    Ok((resolve, pkg_id))
}
