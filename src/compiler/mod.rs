//! TypeScript → MIR → core Wasm → WASI 0.3 component.

mod bind;
mod http;
mod imports;
mod resources;

use crate::{audit, component, link, strip};
use perry_codegen::Exports;
use perry_mir::PlironResultExt as _;
use perry_wit_diagnostics::Result;
use pliron::{context::Context, op::Op, pass::AnalysisManager};
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

pub fn compile_file(
    input_file: &Path,
    options: &CompileOptions,
) -> Result<Compiled, crate::CompileError> {
    let source = fs::read_to_string(input_file).map_err(|err| {
        crate::Diagnostic::io(
            format!("Reading TypeScript source {}", input_file.display()),
            &err,
        )
    })?;
    compile_typescript(&source, &input_file.to_string_lossy(), options)
}

/// Compile against the world selected from `options.wit_dir`.
pub fn compile_typescript(
    source: &str,
    file_name: &str,
    options: &CompileOptions,
) -> Result<Compiled, crate::CompileError> {
    let (resolve, package) = component::wit::resolve_wit(&options.wit_dir)?;
    let world = component::wit::select_world(&resolve, package, options.world.as_deref())?;
    compile_world(
        source,
        file_name,
        &resolve,
        world,
        WorldOptions {
            componentize: !options.core_only,
            ..Default::default()
        },
    )
    .map_err(Into::into)
}

#[derive(Default, Clone, Copy)]
pub(crate) struct WorldOptions {
    pub(crate) componentize: bool,
    pub(crate) memory_checks: bool,
    pub(crate) limits: perry_mir::FactLimits,
}

fn compile_world(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    options: WorldOptions,
) -> Result<Compiled> {
    let modules = perry_frontend::source_files::load(source, file_name)?;
    compile_sources(source, file_name, &modules, resolve, world, options)
}

pub(crate) fn compile_sources(
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    options: WorldOptions,
) -> Result<Compiled> {
    audit::audit_no_llvm(include_str!("../../Cargo.lock"))
        .map_err(|err| err.with_internal_context("LLVM audit verification failed"))?;
    if http::is_handler_world(resolve, world) {
        return http::compile(source, identity, modules, resolve, world, options);
    }
    let ctx = &mut Context::new();
    let mut foreign = perry_frontend::wit::WorldTypes::new(resolve, Some(world));
    let mut bindings = imports::bindings(resolve, world, &mut foreign);
    let resources = resources::definitions(resolve, world, &mut foreign, &mut bindings)?;
    let perry_frontend::LoweredProgram {
        module: mir,
        exports: targets,
    } = perry_frontend::lower_world(
        ctx,
        source,
        identity,
        modules,
        &foreign,
        Some((resolve, world)),
    )?;
    let mut exports = bind::exports(ctx, targets, resolve, world)?;
    let mut analyses = optimize(ctx, &mir, options.limits)?;
    bind::finalize_exports(ctx, &mir, &mut exports)?;
    emit_component(
        ctx,
        &mir,
        &mut analyses,
        resolve,
        world,
        options.memory_checks,
        options.componentize,
        exports,
        bindings,
        resources,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn emit_component(
    ctx: &mut Context,
    mir: &pliron::builtin::ops::ModuleOp,
    analyses: &mut AnalysisManager,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    memory_checks: bool,
    componentize: bool,
    exports: Vec<perry_codegen::boundary::Export>,
    bindings: Vec<perry_codegen::boundary::Import>,
    resources: Vec<perry_codegen::boundary::ResourceDefinition>,
    strip_unused_stdio: bool,
) -> Result<Compiled> {
    let imports = bind::imports(resolve, world);
    let streams = bind::stream_anchor(resolve, world);
    let stdout = !strip_unused_stdio || uses_stdout(ctx, mir);
    let stderr = !strip_unused_stdio || imports.iter().any(|name| name == "wasi:cli/stderr@0.3.0");
    let exports = Exports::Canonical {
        memory_checks,
        exports,
        imports,
        bindings,
        resources,
    };
    let mut allocation_report = perry_codegen::explain_tiers(ctx, mir, &exports, analyses)?;
    let core = perry_codegen::compile(ctx, mir, &exports, analyses)?;
    let (core, helpers) = finish(
        core,
        streams.as_ref(),
        &link::canonical::Requirements::for_world(resolve, world),
        |module, _| match module {
            "wasi:cli/stdout@0.3.0" => !stdout,
            "wasi:cli/stderr@0.3.0" => !stderr,
            _ => false,
        },
    )?;
    use std::fmt::Write as _;
    let _ = writeln!(allocation_report, "Linked helper functions: {helpers}");
    let component = componentize
        .then(|| ComponentOutput::encode(&core, resolve, world))
        .transpose()?;
    Ok(Compiled {
        core,
        component,
        allocation_report,
    })
}

fn compile_core_with(source: &str, file_name: &str, memory_checks: bool) -> Result<Vec<u8>> {
    let modules = perry_frontend::source_files::load(source, file_name)?;
    compile_core_sources(source, file_name, &modules, memory_checks)
}

pub(crate) fn compile_core_sources(
    source: &str,
    identity: &str,
    modules: &BTreeMap<String, String>,
    memory_checks: bool,
) -> Result<Vec<u8>> {
    let ctx = &mut Context::new();
    let (mir, mut analyses) = lower(
        ctx,
        source,
        identity,
        modules,
        &Default::default(),
        Default::default(),
    )?;
    let exports = if memory_checks {
        Exports::RawChecked
    } else {
        Exports::Raw
    };
    let core = perry_codegen::compile(ctx, &mir, &exports, &mut analyses)?;
    let (core, _) = finish(
        core,
        None,
        &link::canonical::Requirements::default(),
        |module, name| {
            name.ends_with(component::reactions::ANCHOR)
                || (module == "$root"
                    && matches!(
                        name,
                        "[waitable-set-new]" | "[waitable-set-drop]" | "[waitable-join]"
                    ))
        },
    )?;
    Ok(core)
}

fn lower(
    ctx: &mut Context,
    source: &str,
    file_name: &str,
    modules: &BTreeMap<String, String>,
    foreign: &perry_frontend::wit::WorldTypes,
    limits: perry_mir::FactLimits,
) -> Result<(pliron::builtin::ops::ModuleOp, AnalysisManager)> {
    let mir = perry_frontend::lower_sources(ctx, source, file_name, modules, foreign)?;
    let analyses = optimize(ctx, &mir, limits)?;
    Ok((mir, analyses))
}

fn optimize(
    ctx: &mut Context,
    mir: &pliron::builtin::ops::ModuleOp,
    limits: perry_mir::FactLimits,
) -> Result<AnalysisManager> {
    if limits != Default::default() {
        limits.set(ctx, mir.get_operation());
    }
    perry_mir::canonicalize(mir.get_operation(), ctx)
        .to_diag(ctx)
        .map_err(|err| err.with_internal_context("optimizing MIR"))
}

fn finish(
    core: Vec<u8>,
    streams: Option<&link::StreamAnchor>,
    canonical: &link::canonical::Requirements,
    stub_runtime_import: impl Fn(&str, &str) -> bool,
) -> Result<(Vec<u8>, u32)> {
    let (core, helpers) = link::link_helpers_with(core, streams, canonical, stub_runtime_import)
        .map_err(|err| err.with_internal_context("linking perry-runtime and helpers"))?;
    audit::validate_guest(&core)
        .map_err(|err| err.with_internal_context("validating stackless core Wasm"))?;
    Ok((core, helpers))
}

fn uses_stdout(ctx: &Context, mir: &pliron::builtin::ops::ModuleOp) -> bool {
    use perry_mir::{Intrinsic, IntrinsicOp};
    perry_mir::functions(mir.get_operation(), ctx)
        .iter()
        .flat_map(|f| perry_mir::operations(ctx, f))
        .filter_map(|op| pliron::operation::Operation::get_op::<IntrinsicOp>(op, ctx))
        .any(|op| {
            matches!(
                op.intrinsic(ctx),
                Intrinsic::PrintOut | Intrinsic::WritableStdout
            )
        })
}

/// Compile against a resolved world, wrapping the output in a component when `componentize`
/// is true.
pub fn compile_for_world(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
) -> Result<Compiled, crate::CompileError> {
    compile_for_world_with_limits(
        source,
        file_name,
        resolve,
        world,
        componentize,
        Default::default(),
    )
}

/// Compile a resolved world with explicit alias and specialization budgets.
pub fn compile_for_world_with_limits(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
    componentize: bool,
    limits: perry_mir::FactLimits,
) -> Result<Compiled, crate::CompileError> {
    compile_world(
        source,
        file_name,
        resolve,
        world,
        WorldOptions {
            componentize,
            limits,
            ..Default::default()
        },
    )
    .map_err(Into::into)
}

/// Compile a component test with forced safe-point collection and reclaimed-memory poisoning.
/// Production compilation omits the instrumentation.
pub fn compile_for_world_with_memory_checks(
    source: &str,
    file_name: &str,
    resolve: &wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<Compiled, crate::CompileError> {
    compile_world(
        source,
        file_name,
        resolve,
        world,
        WorldOptions {
            componentize: true,
            memory_checks: true,
            ..Default::default()
        },
    )
    .map_err(crate::CompileError::from)
}

/// Build a linked core module exporting every exported TypeScript function with its raw core
/// signature (numbers as `f64`, references as addresses, plus an `i32` exception status for
/// functions that may throw). Components use [compile_for_world].
pub fn compile_core(source: &str, file_name: &str) -> Result<Vec<u8>, crate::CompileError> {
    compile_core_with(source, file_name, false).map_err(crate::CompileError::from)
}

/// Compile a raw test module with `__perry_gc_debug(flags)` and
/// `__perry_gc_counter(index)`. Flags enable forced safe-point collection (1) and poisoning
/// (2), plus buffer-growth counters (4). Counter indices are allocations (0), collections (1),
/// live bytes (2), reserved bytes (3), transfer-copy bytes (4), active pins (5), active roots (6),
/// compiled root publications (7), in-place growths (8), copy fallbacks (9), and growth-copy
/// bytes (10). Canonical production components omit these exports.
pub fn compile_core_with_memory_checks(
    source: &str,
    file_name: &str,
) -> Result<Vec<u8>, crate::CompileError> {
    compile_core_with(source, file_name, true).map_err(crate::CompileError::from)
}
