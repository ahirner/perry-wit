//! WASI CLI entrypoint adapter and memory adjustments for Core WebAssembly modules.

use anyhow::Result;

pub(crate) fn synthesize_wasi_cli_entry(wasm_bytes: &[u8]) -> Result<Vec<u8>> {
    let wat = wasmprinter::print_bytes(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("wasmprinter failed: {e}"))?;

    // Ensure memory has enough pages for guest runtime (at least 32 pages = 2MB)
    let mut wat = wat.replace("(memory (;0;) 2)", "(memory (;0;) 32)");
    let pattern = "(export \"_start\" (func ";
    let idx = wat
        .find(pattern)
        .ok_or_else(|| anyhow::anyhow!("Could not find _start export in wat"))?;
    let rest = &wat[idx + pattern.len()..];
    let close = rest
        .find(')')
        .ok_or_else(|| anyhow::anyhow!("Malformed _start export in wat"))?;
    let start_func_ref = rest[..close].trim();
    let clean_func_ref = start_func_ref.trim_matches(|c| c == '(' || c == ';' || c == ')');

    let last_paren = wat
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("No closing paren in wat"))?;

    let wrapper = format!(
        "\n  (func $wasi_cli_run (result i32)\n    call {}\n    i32.const 0\n  )\n  (export \"wasi:cli/run@0.2.6#run\" (func $wasi_cli_run))\n",
        clean_func_ref
    );

    wat.insert_str(last_paren, &wrapper);

    wat::parse_str(&wat)
        .map_err(|e| anyhow::anyhow!("Failed to re-parse wat with wasi:cli/run wrapper: {e}"))
}
