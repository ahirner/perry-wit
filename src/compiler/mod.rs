//! TypeScript → MIR → core Wasm → WASI 0.3 component.

mod bind;
mod http;
mod imports;
mod resources;
mod wit_types;

use crate::{audit, component, link, strip};
use anyhow::{Context as _, Result, anyhow};
use perry_codegen::Exports;
use pliron::{context::Context, op::Op, printable::Printable};
use std::{
    collections::BTreeMap,
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
    pub component: Option<ComponentOutput>,
    pub allocation_report: String,
}

/// Both forms of the same component, present together when componentization is requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentOutput {
    pub bytes: Vec<u8>,
    pub stripped: Vec<u8>,
}

impl ComponentOutput {
    fn encode(
        core: &[u8],
        resolve: &wit_parser::Resolve,
        world: wit_parser::WorldId,
    ) -> Result<Self> {
        let bytes = component::encode_resolved(core, resolve, world)?;
        let stripped = strip::component(&bytes)?;
        Ok(Self { bytes, stripped })
    }
}

fn compile_file_impl(input_file: &Path, options: &CompileOptions) -> Result<Compiled> {
    let source = fs::read_to_string(input_file)
        .with_context(|| format!("Reading TypeScript source {}", input_file.display()))?;
    compile_typescript_impl(&source, &input_file.to_string_lossy(), options)
}

/// Compile against the world selected from `options.wit_dir`.
fn compile_typescript_impl(
    source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled> {
    let (resolve, package) = component::wit::resolve_wit(&options.wit_dir)
        .map_err(|error| crate::CompileError::stage(error, crate::DiagnosticCode::WitResolution))?;
    let world = resolve
        .select_world(&[package], options.world.as_deref())
        .map_err(|error| crate::CompileError::stage(error, crate::DiagnosticCode::WitResolution))?;
    compile_for_world_impl(source, file_name, &resolve, world, !options.core_only)
}

/// Compile against an already resolved world; the component is omitted when `componentize`
/// is false.
fn compile_for_world_impl(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
) -> Result<Compiled> {
    compile_world(source, file_name, resolve, world, componentize, false)
}

/// Compile a component test with forced safe-point collection and reclaimed-memory poisoning.
/// Production compilation omits the instrumentation.
fn compile_for_world_with_memory_checks_impl(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<Compiled> {
    compile_world(source, file_name, resolve, world, true, true)
}

fn compile_world(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
    memory_checks: bool,
) -> Result<Compiled> {
    let modules = perry_lower::source_files::load(source, file_name)?;
    compile_sources(
        source,
        file_name,
        &modules,
        resolve,
        world,
        componentize,
        memory_checks,
    )
}

pub(crate) fn compile_sources(
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
    memory_checks: bool,
) -> Result<Compiled> {
    audit::audit_no_llvm(include_str!("../../Cargo.lock"))
        .context("LLVM audit verification failed")?;
    if http::is_handler_world(resolve, world) {
        return http::compile(
            source,
            identity,
            modules,
            resolve,
            componentize,
            memory_checks,
        );
    }
    let ctx = &mut Context::new();
    let mut foreign = wit_types::foreign(resolve);
    let mut bindings = imports::bindings(resolve, world, &mut foreign);
    let resources = resources::definitions(resolve, world, &mut foreign, &mut bindings)?;
    let mir = lower(ctx, source, identity, modules, &foreign)?;
    let exports = bind::exports(ctx, &mir, resolve, world).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::ExportMismatch)
    })?;
    bind::check_main(ctx, &mir, &exports).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::ExportMismatch)
    })?;
    let imports = bind::imports(resolve, world);
    let streams = bind::stream_anchor(resolve, world);
    let stdout = imports
        .iter()
        .any(|name| name.starts_with("wasi:cli/stdout@"));
    let stderr = imports
        .iter()
        .any(|name| name.starts_with("wasi:cli/stderr@"));
    let exports = Exports::Canonical {
        memory_checks,
        exports,
        imports,
        bindings,
        resources,
    };
    let allocation_report = perry_codegen::explain_tiers(ctx, &mir, &exports);
    let core = perry_codegen::compile(ctx, &mir, &exports).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
    })?;
    let core = finish(core, streams.as_ref()).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
    })?;
    let core = link::raw::available_stdio(core, stdout, stderr).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
    })?;
    let component = componentize
        .then(|| ComponentOutput::encode(&core, resolve, world))
        .transpose()
        .map_err(|error| {
            crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
        })?;
    Ok(Compiled {
        core,
        component,
        allocation_report,
    })
}

/// A linked core module exporting every exported TypeScript function with its raw core
/// signature (numbers as `f64`, references as addresses, plus an `i32` exception status for
/// functions that may throw). For tests and inspection; components use [compile_for_world].
fn compile_core_impl(source: &str, file_name: &str) -> Result<Vec<u8>> {
    let modules = perry_lower::source_files::load(source, file_name)?;
    compile_core_sources(source, file_name, &modules, false)
}

/// Compile a raw test module with `__perry_gc_debug(flags)` and
/// `__perry_gc_counter(index)`. Flags enable forced safe-point collection (1) and poisoning
/// (2), plus buffer-growth counters (4). Counter indices are allocations (0), collections (1),
/// live bytes (2), reserved bytes (3), transfer-copy bytes (4), active pins (5), active roots (6),
/// compiled root publications (7), in-place growths (8), copy fallbacks (9), and growth-copy
/// bytes (10). Canonical production components omit these exports.
fn compile_core_with_memory_checks_impl(source: &str, file_name: &str) -> Result<Vec<u8>> {
    let modules = perry_lower::source_files::load(source, file_name)?;
    compile_core_sources(source, file_name, &modules, true)
}

pub(crate) fn compile_core_sources(
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    memory_checks: bool,
) -> Result<Vec<u8>> {
    let ctx = &mut Context::new();
    let mir = lower(ctx, source, identity, modules, &Default::default())?;
    let exports = if memory_checks {
        Exports::RawChecked
    } else {
        Exports::Raw
    };
    let core = perry_codegen::compile(ctx, &mir, &exports).map_err(|error| {
        crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
    })?;
    link::raw::without_p3(finish(core, None)?)
        .map_err(|error| crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure))
}

fn lower(
    ctx: &mut Context,
    source: &str,
    file_name: &str,
    modules: &BTreeMap<String, String>,
    foreign: &std::collections::HashMap<String, perry_lower::hir::Type>,
) -> Result<pliron::builtin::ops::ModuleOp> {
    let mir =
        perry_lower::lower_sources(ctx, source, file_name, modules, foreign).map_err(|error| {
            crate::CompileError::stage(error, crate::DiagnosticCode::UnsupportedExpression)
        })?;
    perry_mir::pipeline::canonicalize(mir.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))
        .context("optimizing MIR")
        .map_err(|error| {
            crate::CompileError::stage(error, crate::DiagnosticCode::CodegenFailure)
        })?;
    Ok(mir)
}

fn finish(core: Vec<u8>, streams: Option<&link::StreamAnchor>) -> Result<Vec<u8>> {
    let core =
        link::link_helpers_with(core, streams).context("linking perry-runtime and helpers")?;
    let core = link::compact(&core).context("interning core function signatures")?;
    audit::validate_guest(&core).context("validating stackless core Wasm")?;
    Ok(core)
}

pub fn compile_file(
    input_file: &Path,
    options: &CompileOptions,
) -> std::result::Result<Compiled, crate::CompileError> {
    compile_file_impl(input_file, options)
        .map_err(|error| crate::CompileError::from_error(error, crate::DiagnosticCode::Io))
}

pub fn compile_typescript(
    source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> std::result::Result<Compiled, crate::CompileError> {
    compile_typescript_impl(source, file_name, options).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::WitResolution)
    })
}

pub fn compile_for_world(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
) -> std::result::Result<Compiled, crate::CompileError> {
    compile_for_world_impl(source, file_name, resolve, world, componentize).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
    })
}

pub fn compile_for_world_with_memory_checks(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> std::result::Result<Compiled, crate::CompileError> {
    compile_for_world_with_memory_checks_impl(source, file_name, resolve, world).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
    })
}

pub fn compile_core(
    source: &str,
    file_name: &str,
) -> std::result::Result<Vec<u8>, crate::CompileError> {
    compile_core_impl(source, file_name).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
    })
}

pub fn compile_core_with_memory_checks(
    source: &str,
    file_name: &str,
) -> std::result::Result<Vec<u8>, crate::CompileError> {
    compile_core_with_memory_checks_impl(source, file_name).map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
    })
}
