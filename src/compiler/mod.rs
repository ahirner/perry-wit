//! TypeScript compilation pipeline, HIR rewrites, linking, and component packaging.

mod clocks;
mod exceptions;
mod fetch;
mod rewrites;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use perry_codegen_wasm::compile_modules_to_wasm;
use perry_hir::lower_module;
use perry_parser::parse_typescript;

use crate::component;
use crate::linker;
use crate::runtime;
use crate::strip;

/// Options configuring the compilation pipeline.
#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub out_path: Option<PathBuf>,
    pub runtime_path: Option<PathBuf>,
    pub wit_dir: PathBuf,
    pub world: Option<String>,
    pub core_only: bool,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            out_path: None,
            runtime_path: None,
            wit_dir: PathBuf::from("wit"),
            world: Some("merge-docs".to_string()),
            core_only: false,
        }
    }
}

/// Compilation result artifacts.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub raw_core: Vec<u8>,
    pub core: Vec<u8>,
    pub component: Option<Vec<u8>>,
    pub stripped: Option<Vec<u8>>,
}

/// Compiles a TypeScript file according to the provided compile options.
pub fn compile_file(input_file: &Path, options: &CompileOptions) -> Result<Compiled> {
    let ts_content = fs::read_to_string(input_file)
        .with_context(|| format!("Failed to read TypeScript source {}", input_file.display()))?;
    let file_name = input_file
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("module.ts");
    compile_typescript(&ts_content, file_name, options)
}

/// Compiles TypeScript source to an unlinked raw WebAssembly module and its function metadata.
pub fn compile_typescript_raw(
    ts_source: &str,
    file_name: &str,
) -> Result<(Vec<u8>, Vec<(String, u32)>, Vec<perry_hir::ir::Function>)> {
    let mut ast = parse_typescript(ts_source, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse {file_name}: {e:?}"))?;
    fetch::preserve_options(&mut ast);
    clocks::rewrite_performance_now(&mut ast);

    let mut hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {file_name}: {e:?}"))?;

    rewrites::rewrite_program(&mut hir);

    let exported_functions = hir.exported_functions.clone();
    let functions = hir.functions.clone();

    let raw_wasm = compile_modules_to_wasm(&[("main".to_string(), hir)])
        .map_err(|e| anyhow::anyhow!("Compilation failed: {e:?}"))?;
    let raw_wasm = exceptions::lower_runtime_exceptions(&raw_wasm)?;

    // Ensure initial memory has enough pages for guest runtime
    let wat = wasmprinter::print_bytes(&raw_wasm)
        .map_err(|e| anyhow::anyhow!("wasmprinter failed: {e}"))?;
    let wat = wat.replace("(memory (;0;) 2)", "(memory (;0;) 32)");
    let raw_wasm = wat::parse_str(&wat)
        .map_err(|e| anyhow::anyhow!("re-parsing raw wasm with adjusted memory failed: {e}"))?;

    Ok((raw_wasm, exported_functions, functions))
}

/// Compiles TypeScript source text according to the provided compile options.
pub fn compile_typescript(
    ts_source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled> {
    let (raw_wasm, exported_functions, functions) = compile_typescript_raw(ts_source, file_name)?;

    let rt_bytes = runtime::resolve_guest_runtime_bytes(options.runtime_path.as_deref())?;

    let merged_core = linker::merge_core_modules(&raw_wasm, &rt_bytes)
        .context("Linking TypeScript core wasm with guest runtime")?;

    let wit_exports =
        crate::abi::extract_world_exports(&options.wit_dir, options.world.as_deref())?;

    let ready_core = crate::abi::synthesize_trampolines(
        &merged_core,
        &wit_exports,
        &exported_functions,
        &functions,
    )?;

    if options.core_only {
        return Ok(Compiled {
            raw_core: raw_wasm,
            core: ready_core,
            component: None,
            stripped: None,
        });
    }

    let component_bytes =
        component::embed_and_encode(&ready_core, &options.wit_dir, options.world.as_deref())?;

    let stripped_bytes =
        strip::component(&component_bytes).context("Stripping custom sections from component")?;

    Ok(Compiled {
        raw_core: raw_wasm,
        core: ready_core,
        component: Some(component_bytes),
        stripped: Some(stripped_bytes),
    })
}
