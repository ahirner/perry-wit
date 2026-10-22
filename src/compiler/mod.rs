//! TypeScript → MIR → core Wasm → WASI 0.3 component.

mod bind;
mod http;
mod imports;
mod resources;
mod wit_types;

use crate::{audit, component, link, strip};
use anyhow::{Context as _, Result, anyhow};
use perry_codegen::Exports;
use pliron::{context::Context, op::Op, pass::AnalysisManager, printable::Printable};
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
    let (resolve, package) = component::wit::resolve_wit(&options.wit_dir)?;
    let world = component::wit::select_world(&resolve, package, options.world.as_deref())?;
    compile_for_world_impl(source, file_name, &resolve, world, !options.core_only)
}

/// Compile against a resolved world, wrapping the output in a component when `componentize`
/// is true.
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
            world,
            componentize,
            memory_checks,
        );
    }
    let ctx = &mut Context::new();
    let mut foreign = wit_types::foreign(resolve, Some(world));
    let mut bindings = imports::bindings(resolve, world, &mut foreign);
    let resources = resources::definitions(resolve, world, &mut foreign, &mut bindings)?;
    let mir = perry_lower::lower_sources(ctx, source, identity, modules, &foreign)
        .map_err(map_lower_error)
        .map_err(|error| {
            crate::CompileError::from_error(error, crate::DiagnosticCode::UnsupportedExpression)
        })?;
    let mut exports = bind::exports(ctx, &mir, resolve, world)?;
    bind::constrain_numbers(ctx, &mir, &exports)?;
    let mut analyses = optimize(ctx, &mir)?;
    bind::specialized_implementations(ctx, &mir, &mut exports);
    bind::check_main(ctx, &mir, &exports)?;
    Ok(emit_component(
        ctx,
        &mir,
        &mut analyses,
        resolve,
        world,
        memory_checks,
        componentize,
        exports,
        bindings,
        resources,
        true,
    )
    .map_err(|error| {
        crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
    })?)
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
    let (mut has_stdout, mut stderr) = (false, false);
    for name in &imports {
        has_stdout |= name.starts_with("wasi:cli/stdout@");
        stderr |= name.starts_with("wasi:cli/stderr@");
    }
    let stdout = has_stdout && uses_stdout(ctx, mir);
    let exports = Exports::Canonical {
        memory_checks,
        exports,
        imports,
        bindings,
        resources,
    };
    let mut allocation_report = perry_codegen::explain_tiers(ctx, mir, &exports, analyses)?;
    let core = perry_codegen::compile(ctx, mir, &exports, analyses).map_err(map_codegen_error)?;
    let (mut core, helpers) = finish(
        core,
        streams.as_ref(),
        &link::canonical::Requirements::for_world(resolve, world),
    )?;
    if strip_unused_stdio {
        core = link::raw::available_stdio(core, stdout, stderr)?;
    }
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

/// Build a linked core module exporting every exported TypeScript function with its raw core
/// signature (numbers as `f64`, references as addresses, plus an `i32` exception status for
/// functions that may throw). Components use [compile_for_world].
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
    let (mir, mut analyses) = lower(ctx, source, identity, modules, &Default::default())?;
    let exports = if memory_checks {
        Exports::RawChecked
    } else {
        Exports::Raw
    };
    let core = perry_codegen::compile(ctx, &mir, &exports, &mut analyses)
        .map_err(map_codegen_error)
        .map_err(|error| {
            crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
        })?;
    Ok(
        link::raw::without_p3(finish(core, None, &link::canonical::Requirements::default())?.0)
            .map_err(|error| {
                crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
            })?,
    )
}

fn lower(
    ctx: &mut Context,
    source: &str,
    file_name: &str,
    modules: &BTreeMap<String, String>,
    foreign: &std::collections::HashMap<String, perry_lower::hir::Foreign>,
) -> Result<(pliron::builtin::ops::ModuleOp, AnalysisManager)> {
    let mir = perry_lower::lower_sources(ctx, source, file_name, modules, foreign)
        .map_err(map_lower_error)
        .map_err(|error| {
            crate::CompileError::from_error(error, crate::DiagnosticCode::UnsupportedExpression)
        })?;
    let analyses = optimize(ctx, &mir)?;
    Ok((mir, analyses))
}

fn lower_diagnostic(error: perry_lower::LowerError) -> crate::PerryDiagnostic {
    use perry_lower::{LowerErrorKind, ShapeError};
    let code = match &error.kind {
        LowerErrorKind::Parse { .. } => crate::DiagnosticCode::ParseError,
        LowerErrorKind::ModuleResolution { .. } => crate::DiagnosticCode::ModuleResolution,
        LowerErrorKind::ImportSignatureMismatch { .. } => {
            crate::DiagnosticCode::ImportSignatureMismatch
        }
        LowerErrorKind::Shape(shape) => match shape {
            ShapeError::MissingTypeAnnotation => crate::DiagnosticCode::MissingTypeAnnotation,
            ShapeError::UnsupportedType { .. } | ShapeError::TypeMismatch { .. } => {
                crate::DiagnosticCode::UnsupportedType
            }
            ShapeError::IncompatibleRecordLayout { .. } => {
                crate::DiagnosticCode::IncompatibleRecordLayout
            }
            ShapeError::AbsenceMismatch { .. } => crate::DiagnosticCode::AbsenceMismatch,
        },
        LowerErrorKind::MissingRequiredField { .. } => crate::DiagnosticCode::MissingRequiredField,
        LowerErrorKind::UnsupportedOperator { .. } => crate::DiagnosticCode::UnsupportedOperator,
        LowerErrorKind::UnsupportedSpread => crate::DiagnosticCode::UnsupportedSpread,
        LowerErrorKind::UnsupportedExpression(_) => crate::DiagnosticCode::UnsupportedExpression,
    };
    let help = error.help();
    let message = error.message();
    let mut diagnostic =
        crate::PerryDiagnostic::error(code, message).with_optional_span(error.span);
    if let Some(help) = help {
        diagnostic = diagnostic.with_help(help);
    }
    diagnostic
}

fn map_lower_error(error: anyhow::Error) -> anyhow::Error {
    if let Some(lower_err) = error.downcast_ref::<perry_lower::LowerError>().cloned() {
        let diagnostic = lower_diagnostic(lower_err).with_source_notes(error.as_ref());
        return error.context(diagnostic);
    }
    error
}

fn optimize(ctx: &mut Context, mir: &pliron::builtin::ops::ModuleOp) -> Result<AnalysisManager> {
    let analyses = perry_mir::canonicalize(mir.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))
        .context("optimizing MIR")
        .map_err(|error| {
            crate::CompileError::from_error(error, crate::DiagnosticCode::CodegenFailure)
        })?;
    Ok(analyses)
}

fn map_codegen_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<perry_codegen::CodegenError>() {
        Ok(codegen_err) => {
            let message = codegen_err.to_string();
            let diagnostic = match codegen_err {
                perry_codegen::CodegenError::AsyncInitializerRequiresAsyncExport {
                    span, ..
                } => crate::PerryDiagnostic::error(
                    crate::DiagnosticCode::MissingErrorChannel,
                    message,
                )
                .with_optional_span(span)
                .with_help(
                    "Declare this WIT export async so it can await module initialization and its error channel.",
                ),
                perry_codegen::CodegenError::MissingErrorChannel { span, .. } => {
                    crate::PerryDiagnostic::error(
                        crate::DiagnosticCode::MissingErrorChannel,
                        message,
                    )
                    .with_optional_span(span)
                    .with_help(
                        "Declare a WIT result error, or use an async export with wasi:cli/stderr support.",
                    )
                }
                perry_codegen::CodegenError::ExportBinding { span, source, .. } => {
                    crate::PerryDiagnostic::error(crate::DiagnosticCode::ExportMismatch, message)
                        .with_optional_span(span)
                        .with_source_notes(source.as_ref())
                }
            };
            diagnostic.into()
        }
        Err(error) => error,
    }
}

fn finish(
    core: Vec<u8>,
    streams: Option<&link::StreamAnchor>,
    canonical: &link::canonical::Requirements,
) -> Result<(Vec<u8>, u32)> {
    let (core, helpers) = link::link_helpers_with(core, streams, canonical)
        .context("linking perry-runtime and helpers")?;
    let core = link::compact(&core).context("interning core function signatures")?;
    audit::validate_guest(&core).context("validating stackless core Wasm")?;
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
