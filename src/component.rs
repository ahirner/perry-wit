//! In-process WIT embedding and WebAssembly component encoding.

use anyhow::{Context, Result};
use std::path::Path;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

/// Embeds WIT contract metadata into a core WebAssembly module and encodes it into a component.
pub fn embed_and_encode(
    core_wasm: &[u8],
    wit_dir: &Path,
    world_name: Option<&str>,
) -> Result<Vec<u8>> {
    let mut resolve = Resolve::new();
    let (pkg_id, _files) = resolve.push_dir(wit_dir)
        .with_context(|| format!("loading WIT package from {}", wit_dir.display()))?;
    let world_id = resolve.select_world(&[pkg_id], world_name)
        .with_context(|| format!("selecting world {:?}", world_name))?;

    let mut core = core_wasm.to_vec();
    embed_component_metadata(&mut core, &resolve, world_id, StringEncoding::UTF8)
        .context("embedding component metadata")?;

    let component = ComponentEncoder::default()
        .module(&core)
        .context("configuring component encoder")?
        .validate(true)
        .encode()
        .context("encoding component")?;

    Ok(component)
}
