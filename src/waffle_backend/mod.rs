//! LLVM-free Perry HIR → WAFFLE SSA → WASI 0.3 compiler backend.

pub(crate) mod abi;
pub(crate) mod audit;
pub(crate) mod component;
pub(crate) mod exceptions;
pub(crate) mod registry;
pub(crate) mod resolve;
pub(crate) mod ssa;

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

    let ast = parse_typescript(ts_source, file_name)
        .map_err(|e| anyhow::anyhow!("Failed to parse {file_name}: {e:?}"))?;
    let hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {file_name}: {e:?}"))?;

    compile_hir(&hir, options)
}

/// Compiles Perry HIR using the pure WAFFLE SSA backend.
pub fn compile_hir(
    hir: &HirModule,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    let (waffle_mod, contract) = lower_hir_to_waffle(hir)?;
    let waffle_ir = format!("{}", waffle_mod.display());
    let core = waffle_mod
        .to_wasm_bytes()
        .context("Emitting core Wasm bytes from WAFFLE")?;

    let (component_wat, component) = if options.componentize {
        let (wat, bytes) = component::frame_component(&core, &contract)?;
        (Some(wat), Some(bytes))
    } else {
        (None, None)
    };

    Ok(WaffleCompiled {
        hir: hir.clone(),
        waffle_ir,
        core,
        component_wat,
        component,
        uses_p3_clocks: contract.uses_p3_clocks,
        input_kind: contract.input_kind,
    })
}

/// Lower Perry HIR to a validated WAFFLE module.
pub(crate) fn lower_hir_to_waffle(
    hir: &HirModule,
) -> Result<(waffle::Module<'static>, resolve::ResolvedContract)> {
    let contract = resolve::resolve_contract(hir)?;
    let module = ssa::lower_module(hir, &contract)?;
    Ok((module, contract))
}
