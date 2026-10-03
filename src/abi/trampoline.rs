//! Canonical ABI trampoline synthesizer for Core WebAssembly modules.

use std::collections::HashMap;
use std::fmt::Write;

use anyhow::{Context, Result};
use wasmparser::{ExternalKind, Parser, Payload};

use crate::abi::wit_meta::{AbiType, ExportedWitFunction, WitWorldExports, matches_export_name};

/// Discovered exports and function indices from a merged core WebAssembly module.
#[derive(Debug, Clone, Default)]
pub struct DiscoveredExports {
    pub start_func: Option<u32>,
    pub function_types: Vec<wasmparser::FuncType>,
    pub user_functions: HashMap<String, u32>,
    pub cabi_import_string: Option<u32>,
    pub cabi_export_string: Option<u32>,
    pub cabi_export_result_string: Option<u32>,
    pub cabi_import_json: Option<u32>,
    pub cabi_export_json: Option<u32>,
    pub cabi_post_cleanup: Option<u32>,
    pub cabi_post_result_cleanup: Option<u32>,
    pub cabi_check_exception: Option<u32>,
    pub cabi_record_init_checkpoint: Option<u32>,
    pub cabi_reset_invocation_state: Option<u32>,
    pub cabi_register_global_root: Option<u32>,
    pub cabi_reclaim_temporaries: Option<u32>,
    pub cabi_reclaim_callback_temporaries: Option<u32>,
    pub http_reclaim_responses: Option<u32>,
    pub timers_step: Option<u32>,
    pub guest_async_step: Option<u32>,
    pub guest_async_result: Option<u32>,
    pub user_i64_globals: Vec<u32>,
}

/// Parses the export section of a core WebAssembly module to extract known symbols and indices.
pub fn discover_module_exports(wasm_bytes: &[u8]) -> Result<DiscoveredExports> {
    let mut exports = DiscoveredExports::default();
    let mut types = Vec::new();
    let mut function_type_indices = Vec::new();
    let mut i64_globals = std::collections::HashSet::new();
    let mut num_imported_globals = 0u32;

    for payload in Parser::new(0).parse_all(wasm_bytes) {
        match payload? {
            Payload::TypeSection(reader) => {
                types = reader
                    .into_iter_err_on_gc_types()
                    .collect::<Result<Vec<_>, _>>()?;
            }
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    let import = import?;
                    match import.ty {
                        wasmparser::TypeRef::Func(index) => {
                            function_type_indices.push(index);
                        }
                        wasmparser::TypeRef::Global(g) => {
                            if g.content_type == wasmparser::ValType::I64 {
                                i64_globals.insert(num_imported_globals);
                            }
                            num_imported_globals += 1;
                        }
                        _ => {}
                    }
                }
            }
            Payload::FunctionSection(reader) => {
                function_type_indices.extend(reader.into_iter().collect::<Result<Vec<_>, _>>()?);
            }
            Payload::GlobalSection(reader) => {
                let mut g_idx = num_imported_globals;
                for g in reader {
                    let g = g?;
                    if g.ty.content_type == wasmparser::ValType::I64 {
                        i64_globals.insert(g_idx);
                    }
                    g_idx += 1;
                }
            }
            Payload::ExportSection(reader) => {
                for exp in reader {
                    let exp = exp?;
                    match exp.kind {
                        ExternalKind::Func => match exp.name {
                            "_start" => exports.start_func = Some(exp.index),
                            "cabi_import_string" => exports.cabi_import_string = Some(exp.index),
                            "cabi_export_string" => exports.cabi_export_string = Some(exp.index),
                            "cabi_export_result_string" => {
                                exports.cabi_export_result_string = Some(exp.index)
                            }
                            "cabi_import_json" => exports.cabi_import_json = Some(exp.index),
                            "cabi_export_json" => exports.cabi_export_json = Some(exp.index),
                            "cabi_post_cleanup" => exports.cabi_post_cleanup = Some(exp.index),
                            "cabi_check_exception" => {
                                exports.cabi_check_exception = Some(exp.index)
                            }
                            "cabi_post_result_cleanup" => {
                                exports.cabi_post_result_cleanup = Some(exp.index)
                            }
                            "cabi_record_init_checkpoint" => {
                                exports.cabi_record_init_checkpoint = Some(exp.index)
                            }
                            "cabi_reset_invocation_state" => {
                                exports.cabi_reset_invocation_state = Some(exp.index)
                            }
                            "cabi_register_global_root" => {
                                exports.cabi_register_global_root = Some(exp.index)
                            }
                            "cabi_reclaim_temporaries" => {
                                exports.cabi_reclaim_temporaries = Some(exp.index)
                            }
                            "cabi_reclaim_callback_temporaries" => {
                                exports.cabi_reclaim_callback_temporaries = Some(exp.index)
                            }
                            "http_reclaim_responses" => {
                                exports.http_reclaim_responses = Some(exp.index)
                            }
                            "timers_step" => exports.timers_step = Some(exp.index),
                            "guest_async_step" => exports.guest_async_step = Some(exp.index),
                            "guest_async_result" => exports.guest_async_result = Some(exp.index),
                            name => {
                                exports.user_functions.insert(name.to_string(), exp.index);
                            }
                        },
                        ExternalKind::Global => {
                            if exp.name.starts_with("__wasm_global_")
                                && i64_globals.contains(&exp.index)
                            {
                                exports.user_i64_globals.push(exp.index);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    exports.user_i64_globals.sort_unstable();
    exports.user_i64_globals.dedup();
    exports.function_types = function_type_indices
        .into_iter()
        .map(|index| types[index as usize].clone())
        .collect();

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

    // In Perry's compiled wasm, user functions are exported as __wasm_func_{raw_id}.
    // The first __wasm_func_ is __init_strings (pos 0).
    // Subsequent __wasm_func_ are user functions in hir.functions order (pos 1 + i).
    let mut perry_raw_ids: Vec<u32> = discovered
        .user_functions
        .keys()
        .filter_map(|k| k.strip_prefix("__wasm_func_")?.parse::<u32>().ok())
        .collect();
    perry_raw_ids.sort_unstable();

    let mut mapped_functions = Vec::new();

    for wit_fn in &wit_exports.functions {
        let bare_is_unique = wit_exports
            .functions
            .iter()
            .filter(|other| other.name == wit_fn.name)
            .count()
            == 1;
        let (ts_name, fid) = hir_exported_functions
            .iter()
            .find(|(name, _)| {
                matches_export_name(
                    name,
                    &crate::abi::to_kebab_case(&wit_fn.implementation_name),
                )
            })
            .or_else(|| {
                bare_is_unique
                    .then(|| {
                        hir_exported_functions
                            .iter()
                            .find(|(name, _)| matches_export_name(name, &wit_fn.kebab_name))
                    })
                    .flatten()
            })
            .with_context(|| {
                format!(
                    "missing implementation '{}' for WIT export '{}'",
                    wit_fn.implementation_name, wit_fn.core_name
                )
            })?;
        let pos = hir_functions
            .iter()
            .filter(|function| !function.is_async)
            .position(|function| function.id == *fid)
            .with_context(|| {
                format!(
                    "export '{}' must have a synchronous implementation",
                    wit_fn.core_name
                )
            })?;
        let raw_id = perry_raw_ids
            .get(1 + pos)
            .context("missing compiled function")?;
        let wasm_func_index = discovered.user_functions[&format!("__wasm_func_{raw_id}")];
        mapped_functions.push(MappedExportFunction {
            ts_name: ts_name.clone(),
            wit_function: wit_fn.clone(),
            wasm_func_index,
        });
    }

    let wat = wasmprinter::print_bytes(wasm_bytes)
        .map_err(|e| anyhow::anyhow!("wasmprinter failed: {e}"))?;

    let mut wat = wat.replace("(memory (;0;) 2)", "(memory (;0;) 32)");

    let last_paren = wat
        .rfind(')')
        .ok_or_else(|| anyhow::anyhow!("No closing paren in wat"))?;

    let mut snippets = String::new();
    let check_exception = discovered
        .cabi_check_exception
        .context("guest runtime is missing cabi_check_exception")?;

    let record_init_call = discovered
        .cabi_record_init_checkpoint
        .map(|idx| format!("call {idx}\n      "))
        .unwrap_or_default();

    let reset_call = discovered
        .cabi_reset_invocation_state
        .map(|idx| format!("call {idx}\n    "))
        .unwrap_or_default();

    let reclaim_temporaries_call = discovered
        .cabi_reclaim_temporaries
        .map(|idx| format!("call {idx}\n    "))
        .unwrap_or_default();

    let reclaim_http_call = discovered
        .http_reclaim_responses
        .map(|idx| format!("call {idx}\n    "))
        .unwrap_or_default();

    let mut scan_globals_body = String::new();
    if let Some(reg) = discovered.cabi_register_global_root {
        for gidx in &discovered.user_i64_globals {
            write!(scan_globals_body, "global.get {gidx}\n    call {reg}\n    ").unwrap();
        }
    }

    // Emitted init guard
    let start_func_ref = discovered
        .start_func
        .map(|idx| idx.to_string())
        .unwrap_or_else(|| "_start".to_string());

    write!(
        snippets,
        r#"
  (global $perry_init_guard (mut i32) (i32.const 0))
  (func $perry_ensure_init
    global.get $perry_init_guard
    i32.eqz
    if
      call {start_func_ref}
      call {check_exception}
      {record_init_call}i32.const 1
      global.set $perry_init_guard
    end
  )
  (func $perry_scan_globals
    {scan_globals_body})
  (func $perry_safe_reset
    call $perry_scan_globals
    {reset_call}{reclaim_http_call})
"#
    )
    .unwrap();

    let has_pending_work =
        discovered.timers_step.is_some() || discovered.guest_async_step.is_some();
    if has_pending_work {
        let register = discovered
            .cabi_register_global_root
            .context("guest work driver requires cabi_register_global_root")?;
        let reclaim = discovered
            .cabi_reclaim_callback_temporaries
            .context("guest work driver requires cabi_reclaim_callback_temporaries")?;
        let async_step = discovered
            .guest_async_step
            .map(|step| format!("call {step}"))
            .unwrap_or_else(|| "i32.const 0".into());
        let timer_step = discovered
            .timers_step
            .map(|step| format!("call {step}"))
            .unwrap_or_else(|| "i32.const 0".into());
        let async_result = discovered
            .guest_async_result
            .map(|result| format!("call {result}"))
            .unwrap_or_default();
        // No TypeScript frame is live here, except its raw result awaiting ABI lowering.
        write!(
            snippets,
            r#"
  (func $perry_drain_work (param $value i64) (result i64)
    block $finished
      loop $pending
        {async_step}
        i32.eqz
        if (result i32)
          {timer_step}
        else
          i32.const 1
        end
        i32.eqz
        br_if $finished
        local.get $value
        call {register}
        call $perry_scan_globals
        call {reclaim}
        {reclaim_http_call}br $pending
      end
    end
    local.get $value
    {async_result}
  )
"#
        )
        .unwrap();
    }
    let drain_void_work_call = if has_pending_work {
        "i64.const 0x7ffc000000000001\n    call $perry_drain_work\n    drop\n    "
    } else {
        ""
    };

    // Synthesize CLI entry if world expects it
    if wit_exports.has_cli_command {
        write!(
            snippets,
            r#"
  (func $wasi_cli_run (result i32)
    call $perry_ensure_init
    call $perry_safe_reset
    {drain_void_work_call}call {check_exception}
    call $perry_scan_globals
    {reclaim_temporaries_call}{reclaim_http_call}i32.const 0
  )
  (export "wasi:cli/run@0.2.6#run" (func $wasi_cli_run))
"#,
        )
        .unwrap();
    }

    // Synthesize trampolines for each mapped task export function
    for (i, mapped) in mapped_functions.iter().enumerate() {
        let kebab_name = &mapped.wit_function.core_name;
        let sanitized = i.to_string();
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
                AbiType::I32
                | AbiType::U32
                | AbiType::I64
                | AbiType::U64
                | AbiType::F32
                | AbiType::F64
                | AbiType::Bool => {
                    let (core_type, conversion) = match pty {
                        AbiType::I32 => ("i32", "f64.convert_i32_s\n    i64.reinterpret_f64"),
                        AbiType::U32 => ("i32", "f64.convert_i32_u\n    i64.reinterpret_f64"),
                        AbiType::I64 => ("i64", "f64.convert_i64_s\n    i64.reinterpret_f64"),
                        AbiType::U64 => ("i64", "f64.convert_i64_u\n    i64.reinterpret_f64"),
                        AbiType::F32 => ("f32", "f64.promote_f32\n    i64.reinterpret_f64"),
                        AbiType::F64 => ("f64", "i64.reinterpret_f64"),
                        AbiType::Bool => (
                            "i32",
                            "if (result i64)\n      i64.const 0x7ffc000000000004\n    else\n      i64.const 0x7ffc000000000003\n    end",
                        ),
                        _ => unreachable!(),
                    };
                    param_types.push(core_type);
                    import_calls.push(format!("local.get {local_idx}\n    {conversion}"));
                    local_idx += 1;
                }
                AbiType::ResultString | AbiType::Unit => {
                    anyhow::bail!(
                        "unsupported WIT parameter layout for export '{}'",
                        mapped.wit_function.name
                    );
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
                format!("call {helper}")
            }
            AbiType::Unit => {
                let drops = "drop\n    ".repeat(
                    discovered.function_types[target_func as usize]
                        .results()
                        .len(),
                );
                let reclaim = discovered
                    .cabi_reclaim_temporaries
                    .map(|idx| {
                        format!("call $perry_scan_globals\n    call {idx}\n    {reclaim_http_call}")
                    })
                    .unwrap_or_default();
                format!("{drops}{reclaim}")
            }
            AbiType::I32 => "f64.reinterpret_i64\n    i32.trunc_f64_s".into(),
            AbiType::U32 => "f64.reinterpret_i64\n    i32.trunc_f64_u".into(),
            AbiType::I64 => "f64.reinterpret_i64\n    i64.trunc_f64_s".into(),
            AbiType::U64 => "f64.reinterpret_i64\n    i64.trunc_f64_u".into(),
            AbiType::F32 => "f64.reinterpret_i64\n    f32.demote_f64".into(),
            AbiType::F64 => "f64.reinterpret_i64".into(),
            AbiType::Bool => "i64.const 0x7ffc000000000004\n    i64.eq".into(),
        };

        let results_sig = match mapped.wit_function.result {
            AbiType::Unit => "",
            AbiType::I64 | AbiType::U64 => "(result i64)",
            AbiType::F32 => "(result f32)",
            AbiType::F64 => "(result f64)",
            _ => "(result i32)",
        };

        let import_body = import_calls.join("\n    ");
        let drain_work_call = if has_pending_work {
            match discovered.function_types[target_func as usize].results() {
                [] => drain_void_work_call,
                [wasmparser::ValType::I64] => "call $perry_drain_work\n    ",
                _ => {
                    anyhow::bail!("unsupported guest work result layout for export '{kebab_name}'")
                }
            }
        } else {
            ""
        };

        write!(
            snippets,
            r#"
  (func $cabi_trampoline_{sanitized} {params_sig} {results_sig}
    call $perry_ensure_init
    call $perry_safe_reset
    {import_body}
    call {target_func}
    {drain_work_call}call {check_exception}
    {result_handling}
  )
  (export "{kebab_name}" (func $cabi_trampoline_{sanitized}))
"#
        )
        .unwrap();

        let post_cleanup = if mapped.wit_function.result == AbiType::ResultString {
            discovered.cabi_post_result_cleanup
        } else {
            discovered.cabi_post_cleanup
        };
        if let Some(post_cleanup) = post_cleanup
            && matches!(
                mapped.wit_function.result,
                AbiType::String | AbiType::ResultString
            )
        {
            write!(
                snippets,
                r#"
  (func $cabi_post_trampoline_{sanitized} (param i32)
    local.get 0
    call {post_cleanup}
    call $perry_scan_globals
    {reclaim_temporaries_call}  )
  (export "cabi_post_{kebab_name}" (func $cabi_post_trampoline_{sanitized}))
"#
            )
            .unwrap();
        }
    }

    wat.insert_str(last_paren, &snippets);

    wat::parse_str(&wat)
        .map_err(|e| anyhow::anyhow!("Failed to re-parse wat with Canonical ABI trampolines: {e}"))
}
