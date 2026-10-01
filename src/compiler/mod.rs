//! TypeScript compilation pipeline, HIR rewrites, linking, and component packaging.

mod rewrites;
mod wasi;

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

/// Compiles TypeScript source text according to the provided compile options.
pub fn compile_typescript(
    ts_source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled> {
    let ast = parse_typescript(ts_source, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse {file_name}: {e:?}"))?;

    let mut hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {file_name}: {e:?}"))?;

    rewrites::rewrite_program(&mut hir);

    let raw_wasm = compile_modules_to_wasm(&[("main".to_string(), hir)])
        .map_err(|e| anyhow::anyhow!("Compilation failed: {e:?}"))?;

    let core_wasm = wasi::synthesize_wasi_cli_entry(&raw_wasm)?;

    if options.core_only {
        return Ok(Compiled {
            core: core_wasm,
            component: None,
            stripped: None,
        });
    }

    let rt_path = runtime::ensure_guest_runtime(options.runtime_path.as_deref())?;
    let rt_bytes = fs::read(&rt_path)
        .with_context(|| format!("Reading guest runtime from {}", rt_path.display()))?;

    let merged_core = linker::merge_core_modules(&core_wasm, &rt_bytes)
        .context("Linking TypeScript core wasm with guest runtime")?;

    let component_bytes =
        component::embed_and_encode(&merged_core, &options.wit_dir, options.world.as_deref())?;

    let stripped_bytes =
        strip::component(&component_bytes).context("Stripping custom sections from component")?;

    Ok(Compiled {
        core: merged_core,
        component: Some(component_bytes),
        stripped: Some(stripped_bytes),
    })
}
