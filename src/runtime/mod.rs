//! Guest runtime discovery and on-demand compilation.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

/// Resolves or compiles the guest runtime WebAssembly module.
pub fn ensure_guest_runtime(explicit_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        ensure!(
            path.exists(),
            "Specified guest runtime does not exist: {}",
            path.display()
        );
        return Ok(path.to_path_buf());
    }

    let default_path = PathBuf::from("target/wasm32-unknown-unknown/release/guest_runtime.wasm");
    if default_path.exists() && is_guest_runtime_fresh(&default_path) {
        return Ok(default_path);
    }

    let status = Command::new("cargo")
        .args([
            "rustc",
            "--release",
            "--package",
            "guest-runtime",
            "--target",
            "wasm32-unknown-unknown",
            "--",
            "-C",
            "link-arg=--import-memory",
            "-C",
            "link-arg=--global-base=1048576",
            "-C",
            "link-arg=--no-entry",
        ])
        .status()
        .context("Running cargo rustc for guest-runtime")?;

    ensure!(status.success(), "Failed to compile guest-runtime crate");
    ensure!(
        default_path.exists(),
        "guest_runtime.wasm still not found at {}",
        default_path.display()
    );

    Ok(default_path)
}

fn is_guest_runtime_fresh(target: &Path) -> bool {
    let target_time = match std::fs::metadata(target).and_then(|m| m.modified()) {
        Ok(t) => t,
        Err(_) => return false,
    };

    let check_paths = [
        "crates/guest-runtime/Cargo.toml",
        "crates/guest-runtime/src",
    ];
    for p in check_paths {
        let path = Path::new(p);
        if !path.exists() {
            continue;
        }
        if path.is_file() {
            if let Ok(m) = std::fs::metadata(path).and_then(|m| m.modified())
                && m > target_time
            {
                return false;
            }
        } else if path.is_dir()
            && let Ok(entries) = std::fs::read_dir(path)
        {
            for entry in entries.flatten() {
                if let Ok(m) = entry.metadata().and_then(|m| m.modified())
                    && m > target_time
                {
                    return false;
                }
            }
        }
    }
    true
}
