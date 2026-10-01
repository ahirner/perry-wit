//! In-process WIT embedding and WebAssembly component encoding.

use anyhow::{Context, Result};
use std::path::Path;
use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
use wit_parser::Resolve;

/// Check if a directory contains any `.wit` files.
fn has_wit_files(dir: &Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "wit") {
                return true;
            }
        }
    }
    false
}

/// Embeds WIT contract metadata into a core WebAssembly module and encodes it into a component.
pub fn embed_and_encode(
    core_wasm: &[u8],
    wit_dir: &Path,
    world_name: Option<&str>,
) -> Result<Vec<u8>> {
    let mut resolve = Resolve::new();

    // If wit_dir/deps does not exist, check for WASI_WIT_PATH environment variable
    if !wit_dir.join("deps").exists() {
        if let Ok(wasi_wit_path) = std::env::var("WASI_WIT_PATH") {
            let wasi_path = Path::new(&wasi_wit_path);
            if wasi_path.is_dir() {
                // Topological dependency order for WASI Preview 2 packages
                let ordered_pkgs = [
                    "io",
                    "random",
                    "clocks",
                    "filesystem",
                    "sockets",
                    "cli",
                    "http",
                ];
                for pkg in ordered_pkgs {
                    let pkg_dir = wasi_path.join(pkg);
                    if pkg_dir.is_dir() && has_wit_files(&pkg_dir) {
                        resolve.push_dir(&pkg_dir).with_context(|| {
                            format!(
                                "loading dynamic WASI WIT package from {}",
                                pkg_dir.display()
                            )
                        })?;
                    }
                }
            }
        }
    }

    let (pkg_id, _files) = resolve
        .push_dir(wit_dir)
        .with_context(|| format!("loading WIT package from {}", wit_dir.display()))?;
    let world_id = resolve
        .select_world(&[pkg_id], world_name)
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
