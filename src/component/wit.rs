//! WASI Preview 2 and custom WIT package resolution.

use std::path::Path;

use anyhow::{Context, Result};
use wit_parser::{PackageId, Resolve};

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

pub fn resolve_wit(wit_dir: &Path) -> Result<(Resolve, PackageId)> {
    let mut resolve = Resolve::new();

    // If wit_dir/deps does not exist, check for WASI_WIT_PATH environment variable
    if !wit_dir.join("deps").exists()
        && let Ok(wasi_wit_path) = std::env::var("WASI_WIT_PATH")
    {
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

    let (pkg_id, _files) = resolve
        .push_dir(wit_dir)
        .with_context(|| format!("loading WIT package from {}", wit_dir.display()))?;

    Ok((resolve, pkg_id))
}
