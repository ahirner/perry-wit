//! Guest runtime embedding and resolution.
//!
//! Statically embeds `guest_runtime.wasm` built via `build.rs` so that `perry-wit`
//! is a single self-contained binary capable of running without external runtime files
//! or being compiled into a standalone Wasm component itself.
//!
//! An optional runtime path can be explicitly provided at runtime via `--runtime <path>`.

use std::borrow::Cow;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};

/// Statically embedded guest runtime WebAssembly module bytes compiled by `build.rs`.
pub const EMBEDDED_GUEST_RUNTIME: &[u8] = include_bytes!(env!("GUEST_RUNTIME_WASM"));

/// Resolves the guest runtime WebAssembly module bytes.
///
/// Priority:
/// 1. Explicit path passed via CLI flag `--runtime <path>`
/// 2. Statically embedded guest runtime bytes
pub fn resolve_guest_runtime_bytes(explicit_path: Option<&Path>) -> Result<Cow<'static, [u8]>> {
    if let Some(path) = explicit_path {
        ensure!(
            path.exists(),
            "Specified guest runtime does not exist: {}",
            path.display()
        );
        let bytes = fs::read(path)
            .with_context(|| format!("Reading specified guest runtime from {}", path.display()))?;
        return Ok(Cow::Owned(bytes));
    }

    Ok(Cow::Borrowed(EMBEDDED_GUEST_RUNTIME))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_guest_runtime_is_valid_wasm() {
        assert!(EMBEDDED_GUEST_RUNTIME.len() > 4);
        assert_eq!(&EMBEDDED_GUEST_RUNTIME[0..4], b"\0asm");
    }

    #[test]
    fn test_explicit_nonexistent_runtime_fails() {
        let res = resolve_guest_runtime_bytes(Some(Path::new("nonexistent.wasm")));
        assert!(res.is_err());
    }

    #[test]
    fn test_default_resolves_embedded() {
        let bytes = resolve_guest_runtime_bytes(None).expect("embedded runtime should resolve");
        assert_eq!(bytes.as_ref(), EMBEDDED_GUEST_RUNTIME);
    }
}
