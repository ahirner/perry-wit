//! TypeScript → MIR → core Wasm → WASI 0.3 component.

mod bind;

use crate::{audit, component, link, strip};
use anyhow::{Context as _, Result, anyhow};
use perry_codegen::Exports;
use pliron::{context::Context, op::Op, printable::Printable};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct CompileOptions {
    pub wit_dir: PathBuf,
    pub world: Option<String>,
    pub core_only: bool,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            wit_dir: "wit".into(),
            world: None,
            core_only: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub core: Vec<u8>,
    pub component: Option<Vec<u8>>,
    pub stripped: Option<Vec<u8>>,
}

pub fn compile_file(input_file: &Path, options: &CompileOptions) -> Result<Compiled> {
    let source = fs::read_to_string(input_file)
        .with_context(|| format!("Reading TypeScript source {}", input_file.display()))?;
    compile_typescript(&source, &input_file.to_string_lossy(), options)
}

/// Compile against the world selected from `options.wit_dir`.
pub fn compile_typescript(
    source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled> {
    let (resolve, package) = component::wit::resolve_wit(&options.wit_dir)?;
    let world = resolve.select_world(&[package], options.world.as_deref())?;
    compile_for_world(source, file_name, &resolve, world, !options.core_only)
}

/// Compile against an already resolved world; the component is omitted when `componentize`
/// is false.
pub fn compile_for_world(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
) -> Result<Compiled> {
    audit::audit_no_llvm(include_str!("../../Cargo.lock"))
        .context("LLVM audit verification failed")?;
    let ctx = &mut Context::new();
    let entry = if bind::is_command(resolve, world) {
        perry_lower::EntryPoint::Command
    } else {
        perry_lower::EntryPoint::Exports
    };
    let mir = lower(ctx, source, file_name, entry)?;
    let exports = bind::exports(ctx, &mir, resolve, world)?;
    let imports = bind::imports(resolve, world);
    let core = perry_codegen::compile(ctx, &mir, &Exports::Canonical { exports, imports })?;
    let core = finish(core)?;
    let (component, stripped) = if componentize {
        let component = component::encode_resolved(&core, resolve, world)?;
        let stripped = strip::component(&component)?;
        (Some(component), Some(stripped))
    } else {
        (None, None)
    };
    Ok(Compiled {
        core,
        component,
        stripped,
    })
}

/// A linked core module exporting every exported TypeScript function with its raw core
/// signature (numbers as `f64`, references as addresses, plus an `i32` exception status for
/// functions that may throw). For tests and inspection; components use [compile_for_world].
pub fn compile_core(source: &str, file_name: &str) -> Result<Vec<u8>> {
    let ctx = &mut Context::new();
    let mir = lower(ctx, source, file_name, perry_lower::EntryPoint::Exports)?;
    finish(perry_codegen::compile(ctx, &mir, &Exports::Raw)?)
}

fn lower(
    ctx: &mut Context,
    source: &str,
    file_name: &str,
    entry: perry_lower::EntryPoint,
) -> Result<pliron::builtin::ops::ModuleOp> {
    let mir = perry_lower::lower_source(ctx, source, file_name, entry)?;
    perry_mir::pipeline::canonicalize(mir.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))
        .context("optimizing MIR")?;
    Ok(mir)
}

fn finish(core: Vec<u8>) -> Result<Vec<u8>> {
    let core = link::link_helpers(core).context("linking perry-runtime and helpers")?;
    link::compact(&core).context("interning core function signatures")
}
