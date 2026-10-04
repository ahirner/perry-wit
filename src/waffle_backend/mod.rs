//! LLVM-free Perry HIR → WAFFLE SSA → WASI 0.3 compiler backend.

pub(crate) mod abi;
pub(crate) mod allocation;
pub(crate) mod audit;
mod bytes;
pub(crate) mod capabilities;
mod context;
pub(crate) mod control_flow;
mod date;
mod decoder;
pub(crate) mod exceptions;
mod filesystem;
mod http;
mod initialization;
mod json;
pub(crate) mod libraries;
pub(crate) mod link;
mod nullable;
mod objects;
pub(crate) mod promises;
mod random;
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
mod time;
mod values;
mod visit;
mod wit;

use anyhow::{Context, Result};
use perry_hir::ir::Module as HirModule;
use perry_hir::lower_module;
use perry_parser::parse_typescript;

pub use http::handler::HttpHandlerOptions;
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

/// Compiles TypeScript to core Wasm with `componentize: false`.
/// Use [`compile_typescript_for_world`] for component output with authoritative WIT.
pub fn compile_typescript(
    ts_source: &str,
    file_name: &str,
    options: &WaffleCompileOptions,
) -> Result<WaffleCompiled> {
    compile_source(ts_source, file_name, options, None, None)
}

/// Compiles a bounded HTTP handler exporting `wasi:http/handler@0.3.0`.
pub fn compile_http_handler(
    ts_source: &str,
    file_name: &str,
    options: &WaffleCompileOptions,
    limits: HttpHandlerOptions,
) -> Result<WaffleCompiled> {
    compile_source(
        ts_source,
        file_name,
        options,
        Some(http::handler::world()?),
        Some(limits),
    )
}

/// Compiles against a resolved WIT world, using the generated SDK implementation names.
pub fn compile_typescript_for_world(
    ts_source: &str,
    file_name: &str,
    options: &WaffleCompileOptions,
    resolve: wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<WaffleCompiled> {
    compile_source(
        ts_source,
        file_name,
        options,
        Some(wit::WitWorld::new(resolve, world)?),
        None,
    )
}

fn compile_source(
    ts_source: &str,
    file_name: &str,
    options: &WaffleCompileOptions,
    mut exports: Option<wit::WitWorld>,
    http_handler: Option<HttpHandlerOptions>,
) -> Result<WaffleCompiled> {
    if options.audit_dependencies {
        audit::audit_no_llvm(include_str!("../../Cargo.lock"))
            .context("LLVM audit verification failed")?;
    }

    let mut ast = if exports.is_some() {
        source::modules::load(ts_source, file_name)?
    } else {
        parse_typescript(ts_source, file_name)
            .map_err(|e| anyhow::anyhow!("Failed to parse {file_name}: {e:?}"))?
    };
    text_contract::validate_ast_text(&ast).context("Source text contract validation failed")?;
    if exports.is_some() {
        wit::validate_source(&ast)?;
    }
    if let Some(exports) = &mut exports {
        initialization::prepare_command(&mut ast, exports)?;
    }
    let bindings = source::resolve_bindings(&mut ast, exports.as_ref())?;
    let hir = lower_module(&ast, "main", file_name)
        .map_err(|e| anyhow::anyhow!("Failed to lower {file_name}: {e:?}"))?;
    source::validate_lowering(&hir)?;

    compile_resolved_hir(hir, options, &bindings, exports, http_handler)
}

/// Compiles owned Perry HIR to core Wasm with `componentize: false`.
/// Encode the core with an explicit WIT world using [`encode_component`].
pub fn compile_hir_owned(hir: HirModule, options: &WaffleCompileOptions) -> Result<WaffleCompiled> {
    compile_resolved_hir(hir, options, &source::SourceBindings::default(), None, None)
}

fn compile_resolved_hir(
    mut hir: HirModule,
    options: &WaffleCompileOptions,
    bindings: &source::SourceBindings,
    exports: Option<wit::WitWorld>,
    http_handler: Option<HttpHandlerOptions>,
) -> Result<WaffleCompiled> {
    if exports.is_some() {
        initialization::extract(&mut hir)?;
    }
    anyhow::ensure!(
        !options.componentize || exports.is_some(),
        "Component output requires an explicitly resolved WIT world; use compile_typescript_for_world or request core-only output"
    );
    objects::resolve_declared_types(&mut hir)?;
    if let Some(exports) = &exports {
        exports.validate(&hir)?;
    }
    values::resolve_types(&mut hir);
    text_contract::validate_hir_text(&hir).context("HIR text contract validation failed")?;

    let mut contract = resolve::resolve_contract(&hir, bindings, exports)?;
    if http_handler.is_none()
        && let Some(exports) = &contract.wit
    {
        exports.validate_suspension(&hir, &contract)?;
    }
    contract.http_handler = http_handler;
    let native_handler = http_handler
        .map(|_| http::handler::native_world())
        .transpose()?;
    let mut waffle_mod = ssa::lower_module(&hir, &contract)?;
    if let Some(exports) = native_handler.as_ref().or(contract.wit.as_ref()) {
        exports.bind_native_imports(&mut waffle_mod)?;
    }
    let waffle_ir = format!("{}", waffle_mod.display());
    let core = waffle_mod
        .to_wasm_bytes()
        .context("Emitting core Wasm bytes from WAFFLE")?;
    let core = link::link_helpers(&core).context("Linking guest helpers into core Wasm")?;

    let (component_wat, component) = if options.componentize {
        let (wat, bytes) = native_handler
            .as_ref()
            .or(contract.wit.as_ref())
            .unwrap()
            .frame(&core)?;
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

/// Compiles borrowed Perry HIR to core Wasm with `componentize: false`.
pub fn compile_hir(hir: &HirModule, options: &WaffleCompileOptions) -> Result<WaffleCompiled> {
    compile_hir_owned(hir.clone(), options)
}

/// Encodes a core module against an explicit WIT world using canonical P3 bindings.
pub fn encode_component(
    core: &[u8],
    resolve: wit_parser::Resolve,
    world: wit_parser::WorldId,
) -> Result<Vec<u8>> {
    let wit = wit::WitWorld::for_encoding(resolve, world)?;
    let mut module = waffle::Module::from_wasm_bytes(core, &Default::default())?;
    wit.bind_native_imports(&mut module)?;
    let (_, component) = wit.frame(&module.to_wasm_bytes()?)?;
    Ok(component)
}
