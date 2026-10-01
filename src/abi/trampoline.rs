//! Canonical ABI trampoline synthesizer.

use anyhow::Result;

/// Synthesizes Canonical ABI trampolines for exported functions.
pub fn synthesize_trampolines(_wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    Ok(Vec::new())
}
