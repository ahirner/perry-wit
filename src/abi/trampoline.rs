//! Canonical ABI trampoline synthesizer for Core WebAssembly modules.

use std::collections::HashMap;

use anyhow::{Context, Result};
use wasmparser::{ExternalKind, Parser, Payload};

use crate::abi::wit_meta::{AbiType, ExportedWitFunction, WitWorldExports, matches_export_name};

/// Discovered exports and function indices from a merged core WebAssembly module.
#[derive(Debug, Clone, Default)]
pub struct DiscoveredExports {
    pub start_func: Option<u32>,
    pub user_functions: HashMap<String, u32>,
    pub cabi_import_string: Option<u32>,
    pub cabi_export_string: Option<u32>,
    pub cabi_export_result_string: Option<u32>,
    pub cabi_import_json: Option<u32>,
    pub cabi_export_json: Option<u32>,
    pub cabi_post_cleanup: Option<u32>,
}

/// Parses the export section of a core WebAssembly module to extract known symbols and indices.
pub fn discover_module_exports(wasm_bytes: &[u8]) -> Result<DiscoveredExports> {
    let mut exports = DiscoveredExports::default();

    for payload in Parser::new(0).parse_all(wasm_bytes) {
        if let Payload::ExportSection(reader) = payload? {
            for exp in reader {
                let exp = exp?;
                if exp.kind == ExternalKind::Func {
                    match exp.name {
                        "_start" => exports.start_func = Some(exp.index),
                        "cabi_import_string" => exports.cabi_import_string = Some(exp.index),
                        "cabi_export_string" => exports.cabi_export_string = Some(exp.index),
                        "cabi_export_result_string" => {
                            exports.cabi_export_result_string = Some(exp.index)
                        }
                        "cabi_import_json" => exports.cabi_import_json = Some(exp.index),
                        "cabi_export_json" => exports.cabi_export_json = Some(exp.index),
                        "cabi_post_cleanup" => exports.cabi_post_cleanup = Some(exp.index),
                        name => {
                            exports.user_functions.insert(name.to_string(), exp.index);
                        }
                    }
                }
            }
        }
    }

    Ok(exports)
}

/// Metadata mapping a TypeScript function export to its compiled WebAssembly function index.
#[derive(Debug, Clone)]
pub struct MappedExportFunction {
    pub ts_name: String,
    pub wit_function: ExportedWitFunction,
    pub wasm_func_index: u32,
}

/// Synthesizes Canonical ABI trampolines and entrypoints onto a merged core WebAssembly module.
pub fn synthesize_trampolines(
    wasm_bytes: &[u8],
    wit_exports: &WitWorldExports,
    hir_exported_functions: &[(String, u32)],
    hir_functions: &[perry_hir::ir::Function],
) -> Result<Vec<u8>> {
    let discovered =
        discover_module_exports(wasm_bytes).context("discovering exports from merged core wasm")?;

    // Determine the minimum user func index from __wasm_func_<idx>
    let mut user_func_indices: Vec<u32> = discovered
        .user_functions
        .keys()
        .filter_map(|k| k.strip_prefix("__wasm_func_")?.parse::<u32>().ok())
        .collect();
    user_func_indices.sort_unstable();

    let mut mapped_functions = Vec::new();

    for (ts_name, fid) in hir_exported_functions {
        // Find matching WIT export
        if let Some(wit_fn) = wit_exports
            .functions
            .iter()
            .find(|wf| matches_export_name(ts_name, &wf.kebab_name))
            && let Some(pos) = hir_functions
                .iter()
                .filter(|f| !f.is_async)
                .position(|f| f.id == *fid)
            && let Some(&wasm_func_idx) = user_func_indices.get(pos)
        {
            mapped_functions.push(MappedExportFunction {
                ts_name: ts_name.clone(),
                wit_function: wit_fn.clone(),
                wasm_func_index: wasm_func_idx,
            });
        }
    }

    let wat = wasmprinter::print_bytes(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("wasmprinter failed: {e}"))?;

    // Ensure memory has enough pages for guest runtime (at least 32 pages = 2MB)
    let mut wat = wat.replace("(memory (;0;) 2)", "(memory (;0;) 32)");

    let last_paren = wat
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("No closing paren in wat"))?;

    let mut snippets = String::new();

    // Emitted init guard
    let start_func_ref = discovered
        .start_func
        .map(|idx| idx.to_string())
        .unwrap_or_else(|| "_start".to_string());

    snippets.push_str(&format!(
        r#"
  (global $perry_init_guard (mut i32) (i32.const 0))
  (func $perry_ensure_init
    global.get $perry_init_guard
    i32.eqz
    if
      call {start_func_ref}
      i32.const 1
      global.set $perry_init_guard
    end
  )
"#
    ));

    // Synthesize CLI entry if world expects it
    if wit_exports.has_cli_command {
        snippets.push_str(
            r#"
  (func $wasi_cli_run (result i32)
    call $perry_ensure_init
    i32.const 0
  )
  (export "wasi:cli/run@0.2.6#run" (func $wasi_cli_run))
"#,
        );
    }

    // Synthesize trampolines for each mapped task export function
    for (i, mapped) in mapped_functions.iter().enumerate() {
        let kebab_name = &mapped.wit_function.kebab_name;
        let sanitized = format!("{}_{i}", kebab_name.replace('-', "_"));
        let target_func = mapped.wasm_func_index;

        let mut param_types = Vec::new();
        let mut import_calls = Vec::new();

        let mut local_idx = 0;
        for (_, pty) in &mapped.wit_function.params {
            match pty {
                AbiType::String => {
                    param_types.push("i32 i32");
                    let helper = discovered
                        .cabi_import_string
                        .map(|idx| idx.to_string())
                        .unwrap_or_else(|| "cabi_import_string".to_string());
                    import_calls.push(format!(
                        "local.get {local_idx}\n    local.get {}\n    call {helper}",
                        local_idx + 1
                    ));
                    local_idx += 2;
                }
                AbiType::JsonRecord => {
                    param_types.push("i32 i32");
                    let helper = discovered
                        .cabi_import_json
                        .map(|idx| idx.to_string())
                        .unwrap_or_else(|| "cabi_import_json".to_string());
                    import_calls.push(format!(
                        "local.get {local_idx}\n    local.get {}\n    call {helper}",
                        local_idx + 1
                    ));
                    local_idx += 2;
                }
                AbiType::I32 | AbiType::Bool => {
                    param_types.push("i32");
                    import_calls.push(format!("local.get {local_idx}\n    i64.extend_i32_u"));
                    local_idx += 1;
                }
                AbiType::I64 => {
                    param_types.push("i64");
                    import_calls.push(format!("local.get {local_idx}"));
                    local_idx += 1;
                }
                _ => {
                    param_types.push("i32 i32");
                    let helper = discovered
                        .cabi_import_string
                        .map(|idx| idx.to_string())
                        .unwrap_or_else(|| "cabi_import_string".to_string());
                    import_calls.push(format!(
                        "local.get {local_idx}\n    local.get {}\n    call {helper}",
                        local_idx + 1
                    ));
                    local_idx += 2;
                }
            }
        }

        let params_sig = if param_types.is_empty() {
            String::new()
        } else {
            format!("(param {})", param_types.join(" "))
        };

        let result_handling = match mapped.wit_function.result {
            AbiType::String => {
                let helper = discovered
                    .cabi_export_string
                    .map(|idx| idx.to_string())
                    .unwrap_or_else(|| "cabi_export_string".to_string());
                format!("call {helper}")
            }
            AbiType::ResultString => {
                let helper = discovered
                    .cabi_export_result_string
                    .map(|idx| idx.to_string())
                    .unwrap_or_else(|| "cabi_export_result_string".to_string());
                format!("i32.const 0\n    call {helper}")
            }
            AbiType::JsonRecord => {
                let helper = discovered
                    .cabi_export_json
                    .map(|idx| idx.to_string())
                    .unwrap_or_else(|| "cabi_export_json".to_string());
                format!("call {helper}")
            }
            AbiType::Unit => "drop".to_string(),
            _ => {
                let helper = discovered
                    .cabi_export_string
                    .map(|idx| idx.to_string())
                    .unwrap_or_else(|| "cabi_export_string".to_string());
                format!("call {helper}")
            }
        };

        let results_sig = if mapped.wit_function.result == AbiType::Unit {
            String::new()
        } else {
            "(result i32)".to_string()
        };

        let import_body = import_calls.join("\n    ");

        snippets.push_str(&format!(
            r#"
  (func $cabi_trampoline_{sanitized} {params_sig} {results_sig}
    call $perry_ensure_init
    {import_body}
    call {target_func}
    {result_handling}
  )
  (export "{kebab_name}" (func $cabi_trampoline_{sanitized}))
"#
        ));

        if let Some(post_cleanup) = discovered.cabi_post_cleanup
            && mapped.wit_function.result != AbiType::Unit
        {
            snippets.push_str(&format!(
                r#"
  (func $cabi_post_trampoline_{sanitized} (param i32)
    local.get 0
    call {post_cleanup}
  )
  (export "cabi_post_{kebab_name}" (func $cabi_post_trampoline_{sanitized}))
"#
            ));
        }
    }

    wat.insert_str(last_paren, &snippets);

    wat::parse_str(&wat)
        .map_err(|e| anyhow::anyhow!("Failed to re-parse wat with Canonical ABI trampolines: {e}"))
}
