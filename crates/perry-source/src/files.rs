//! Filesystem acquisition for the native and WASI command adapters.

use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

/// Acquire the entry's local dependencies before running the in-memory module resolver.
pub fn load(source: &str, entry: &str) -> Result<BTreeMap<String, String>> {
    let mut sources = BTreeMap::<String, String>::new();
    let entry = super::modules::normalize(Path::new(entry));
    let mut pending = vec![(entry.clone(), Vec::new())];
    let mut visited = BTreeSet::new();
    while let Some((identity, mut ancestors)) = pending.pop() {
        let canonical = identity.canonicalize().unwrap_or_else(|_| identity.clone());
        ensure!(
            !ancestors.contains(&canonical),
            "cyclic module import: {}",
            identity.display()
        );
        if !visited.insert(identity.clone()) {
            continue;
        }
        ancestors.push(canonical);
        let text = if identity == entry {
            source
        } else {
            sources
                .get(identity.to_string_lossy().as_ref())
                .with_context(|| format!("Missing queued source: {}", identity.display()))?
        };
        let module = crate::parse(text, &identity.to_string_lossy())?;
        for import in super::modules::imports(&module) {
            let Some(path) = super::modules::candidates(&identity, &import)?
                .into_iter()
                .find(|path| path.is_file())
            else {
                continue;
            };
            if sources.contains_key(path.to_string_lossy().as_ref()) {
                continue;
            }
            let contents =
                fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
            sources.insert(path.to_string_lossy().into_owned(), contents);
            pending.push((path, ancestors.clone()));
        }
    }
    Ok(sources)
}

/// Read a source tree and assign package-relative identities for the content API.
pub fn read_program(entry: &Path) -> Result<crate::Program> {
    let entry = entry
        .canonicalize()
        .with_context(|| format!("Reading {}", entry.display()))?;
    let source = fs::read_to_string(&entry)?;
    let modules = load(&source, &entry.to_string_lossy())?;
    let mut root = entry
        .parent()
        .context("Source file has no parent directory")?;
    for name in modules.keys() {
        while !Path::new(name).starts_with(root) {
            root = root
                .parent()
                .context("Source modules have no common root")?;
        }
    }
    let name = |path: &Path| -> Result<String> {
        Ok(path
            .strip_prefix(root)?
            .to_string_lossy()
            .replace('\\', "/"))
    };
    Ok(crate::Program {
        source: crate::Source {
            name: Some(name(&entry)?),
            contents: source.into_bytes(),
        },
        modules: modules
            .into_iter()
            .map(|(path, contents)| {
                Ok(crate::Module {
                    name: name(Path::new(&path))?,
                    contents: contents.into_bytes(),
                })
            })
            .collect::<Result<_>>()?,
    })
}
