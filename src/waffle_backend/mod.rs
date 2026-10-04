//! LLVM-free Perry HIR → WAFFLE SSA → WASI 0.3 compiler backend.

pub(crate) mod abi;
pub(crate) mod allocation;
pub(crate) mod audit;
mod bytes;
pub(crate) mod capabilities;
pub(crate) mod component;
pub(crate) mod control_flow;
mod decoder;
pub(crate) mod exceptions;
mod filesystem;
pub(crate) mod libraries;
pub(crate) mod link;
mod objects;
pub(crate) mod promises;
mod regex;
pub(crate) mod registry;
pub(crate) mod resolve;
mod runtime;
mod source;
pub(crate) mod ssa;
mod streams;
pub(crate) mod strings;
mod structured;
pub mod text_contract;
mod text_or_bytes;
mod visit;

use anyhow::{Context, Result};
use perry_hir::ir::Module as HirModule;
use perry_hir::lower_module;
use perry_parser::parse_typescript;

pub use resolve::ResolvedInputKind;

/// Compilation options for the WAFFLE backend.
#[derive(Debug, Clone)]
pub struct WaffleCompileOptions {
    pub componentize: bool,
    pub audit_dependencies: bool,
}

impl Default for WaffleCompileOptions {
    fn default() -> Self {
        Self {
            componentize: true,
            audit_dependencies: true,
        }
    }
}

/// Inspectable artifacts produced by the WAFFLE compilation path.
#[derive(Clone, Debug)]
pub struct WaffleCompiled {
    pub hir: HirModule,
    pub waffle_ir: String,
    pub core: Vec<u8>,
    pub component_wat: Option<String>,
    pub component: Option<Vec<u8>>,
    pub uses_p3_clocks: bool,
    pub input_kind: ResolvedInputKind,
}

/// Compiles TypeScript source using the pure WAFFLE SSA backend.
pub fn compile_typescript(
    ts_source: &str,
    file_name: &str,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    if options.audit_dependencies {
        audit::audit_no_llvm(include_str!("../../Cargo.lock"))
            .context("LLVM audit verification failed")?;
    }

    let mut ast = parse_typescript(ts_source, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse {file_name}: {e:?}"))?;
    text_contract::validate_ast_text(&ast).context("Source text contract validation failed")?;
    let bindings = source::resolve_bindings(&mut ast)?;
    let hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {file_name}: {e:?}"))?;
    source::validate_lowering(&hir)?;

    compile_resolved_hir(hir, options, &bindings)
}

/// Compiles Perry HIR by taking ownership, avoiding redundant cloning of the HIR.
pub fn compile_hir_owned(hir: HirModule, options: &WaffleCompileOptions) -> Result<WaffleCompiled> {
    compile_resolved_hir(hir, options, &source::SourceBindings::default())
}

fn compile_resolved_hir(
    mut hir: HirModule,
    options: &WaffleCompileOptions,
    bindings: &source::SourceBindings,
) -> Result<WaffleCompiled> {
    objects::resolve_declared_types(&mut hir)?;
    text_contract::validate_hir_text(&hir).context("HIR text contract validation failed")?;

    let contract = resolve::resolve_contract(&hir, bindings)?;
    let waffle_mod = ssa::lower_module(&hir, &contract)?;
    let waffle_ir = format!("{}", waffle_mod.display());
    let core = waffle_mod
        .to_wasm_bytes()
        .context("Emitting core Wasm bytes from WAFFLE")?;
    let core = link::link_helpers(&core).context("Linking guest helpers into core Wasm")?;

    let (component_wat, component) = if options.componentize {
        let has_post_return = waffle_mod
            .exports
            .iter()
            .any(|export| export.name == "cabi_post_run");
        let (wat, bytes) = component::frame_component(&core, &contract, has_post_return)?;
        (Some(wat), Some(bytes))
    } else {
        (None, None)
    };

    Ok(WaffleCompiled {
        hir,
        waffle_ir,
        core,
        component_wat,
        component,
        uses_p3_clocks: contract.uses_p3_clocks,
        input_kind: contract.input_kind,
    })
}

/// Compiles Perry HIR using the pure WAFFLE SSA backend.
pub fn compile_hir(hir: &HirModule, options: &WaffleCompileOptions) -> Result<WaffleCompiled> {
    compile_hir_owned(hir.clone(), options)
}
