//! In-process WIT embedding and WebAssembly component encoding.

pub(crate) mod reactions;
pub mod wit;

use std::{borrow::Cow, path::Path};

use anyhow::{Context, Result};
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};

/// Embeds WIT contract metadata into a core WebAssembly module and encodes it into a component.
pub fn embed_and_encode(
    core_wasm: &[u8],
    wit_dir: &Path,
    world_name: Option<&str>,
) -> Result<Vec<u8>> {
    let (resolve, pkg_id) = wit::resolve_wit(wit_dir)?;
    let world_id = resolve
        .select_world(&[pkg_id], world_name)
        .with_context(|| format!("selecting world {:?}", world_name))?;

    encode_resolved(core_wasm, &resolve, world_id)
}

/// Encode the resolved world directly. Modules using promise signals carry the
/// private [reactions::ANCHOR] export only during component encoding.
pub(crate) fn encode_resolved(
    core_wasm: &[u8],
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<Vec<u8>> {
    let signals = reactions::uses_signals(core_wasm)?;
    let (mut core, resolve) = if signals {
        let (core, resolve) = reactions::prepare(core_wasm, resolve, world)?;
        (core, Cow::Owned(resolve))
    } else {
        (core_wasm.to_vec(), Cow::Borrowed(resolve))
    };
    embed_component_metadata(&mut core, &resolve, world, StringEncoding::UTF8, false)
        .context("embedding component metadata")?;
    let component = ComponentEncoder::default()
        .module(&core)
        .context("configuring component encoder")?
        .validate(true)
        .encode()
        .context("encoding component")?;
    let component = if signals {
        reactions::hide(&component)?
    } else {
        component
    };
    crate::audit::validate_guest(&component).context("validating stackless component")?;
    Ok(component)
}
