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
    if default_path.exists() {
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
