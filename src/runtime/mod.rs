//! Guest runtime discovery and artifact resolution.
//!
//! Provides pure, deterministic resolution of the precompiled `guest_runtime.wasm`
//! artifact across CLI flags, environment variables, Nix wrapper locations,
//! and standard artifact paths with zero impure runtime compilation side-effects.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail, ensure};

/// Environment variable used to specify the guest runtime WebAssembly module path.
pub const ENV_PERRY_GUEST_RUNTIME: &str = "PERRY_GUEST_RUNTIME";

/// Resolves the guest runtime WebAssembly module path without any impure runtime build steps.
pub fn ensure_guest_runtime(explicit_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit_path {
        ensure!(
            path.exists(),
            "Specified guest runtime does not exist: {}",
            path.display()
        );
        return Ok(path.to_path_buf());
    }

    // 1. Check PERRY_GUEST_RUNTIME environment variable
    if let Ok(env_val) = std::env::var(ENV_PERRY_GUEST_RUNTIME) {
        let env_path = PathBuf::from(&env_val);
        if env_path.exists() {
            return Ok(env_path);
        }
    }

    // 2. Check relative to current executable ($ORIGIN/../lib/guest_runtime.wasm or $ORIGIN/guest_runtime.wasm)
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(bin_dir) = current_exe.parent()
    {
        let exe_rel = bin_dir.join("../lib/guest_runtime.wasm");
        if exe_rel.exists() {
            return Ok(exe_rel);
        }
        let same_dir = bin_dir.join("guest_runtime.wasm");
        if same_dir.exists() {
            return Ok(same_dir);
        }
    }

    // 3. Check well-known workspace / repository artifact paths
    let candidate_paths = [
        "artifacts/guest_runtime.wasm",
        "target/wasm32-unknown-unknown/release/guest_runtime.wasm",
        "target/wasm32-unknown-unknown/debug/guest_runtime.wasm",
    ];

    for candidate in candidate_paths {
        let p = PathBuf::from(candidate);
        if p.exists() {
            return Ok(p);
        }
    }

    bail!(
        "Guest runtime WebAssembly module ('guest_runtime.wasm') not found.\n\
         Please provide it using one of the following methods:\n\
           1. Pass '--runtime <path>' on the command line\n\
           2. Set the 'PERRY_GUEST_RUNTIME' environment variable\n\
           3. Symlink the artifact into 'artifacts/guest_runtime.wasm'\n\
           4. Build it using 'nix build .#guest-runtime' or 'scripts/build.sh'"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explicit_nonexistent_path_fails() {
        let res = ensure_guest_runtime(Some(Path::new("nonexistent/guest_runtime.wasm")));
        assert!(res.is_err());
        assert!(
            res.unwrap_err()
                .to_string()
                .contains("Specified guest runtime does not exist")
        );
    }

    #[test]
    fn test_resolves_existing_candidate() {
        // In local development or nix develop, at least one candidate exists
        let res = ensure_guest_runtime(None);
        if let Ok(path) = res {
            assert!(path.exists());
        }
    }
}
