//! Filesystem acquisition for the native and WASI command adapters.

use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

/// Acquire the entry's local dependencies before running the in-memory module resolver.
pub fn load(source: &str, entry: &str) -> Result<BTreeMap<String, String>> {
    let mut sources = BTreeMap::new();
    let mut pending = vec![(
        super::modules::normalize(Path::new(entry)),
        source.to_owned(),
        Vec::new(),
    )];
    let mut visited = BTreeSet::new();
    while let Some((identity, source, mut ancestors)) = pending.pop() {
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
        let module = crate::hir_compat::parse(&source, &identity.to_string_lossy())?;
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
            sources.insert(path.to_string_lossy().into_owned(), contents.clone());
            pending.push((path, contents, ancestors.clone()));
        }
    }
    Ok(sources)
}
