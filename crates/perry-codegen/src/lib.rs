//! perry MIR → pliron `wasm` dialect → core Wasm.
//!
//! The output defines and exports linear memory when required, places string constants in data
//! segments starting at `DATA_BASE`, and imports runtime functions from `HELPER_MODULE` for
//! `perry-wit`'s linker to resolve against `perry-runtime` and the helper libraries. Exports
//! are either raw functions ([Exports::Raw]) or canonical-ABI component exports
//! ([Exports::Canonical], see [boundary]). Value and storage representations follow these rules:
//!
//! - `f64` stays `f64`, and semantic integers use `i32` through 32 bits and `i64` through 64
//!   bits, with narrow results normalized at their wrap points. Records, arrays, strings, and
//!   promises are `i32` addresses.
//! - `undefined` is the signalling-NaN bit pattern [perry_abi::UNDEFINED_BITS] for
//!   numbers, the null address for references, and `2` for booleans. Wasm arithmetic only
//!   produces quiet NaNs, and `perry.to_number` canonicalizes inputs before arithmetic.
//! - Records consist of an `i32` shape id and a parse-source word followed by 8-byte field
//!   slots in declaration order. Arrays and variants use the layouts in [perry_abi::layout].
//! - Unproven and repeated allocations use precise collectible storage. Bounded task storage
//!   remains available for small synchronous exports and task-owned handle tables.
//! - Strings are UTF-8 with a cached Unicode scalar count (layout in [perry_abi::layout::string]),
//!   and string constants are static objects in the data segment.
//! - Exceptions use the status convention: a function that may throw returns an extra `i32`
//!   status after its results (0 for normal completion, or an [ErrorKind] code), and callers
//!   branch on it. Export wrappers map the status to the declared WIT result or trap.
//!
//! `module` drives the lowering of a whole module and `function` lowers individual functions,
//! dispatching operations through `ops` to the topical modules. `async_split` transforms async
//! functions into state machines, and [boundary] binds them to the canonical ABI.
use anyhow::{Context as _, Result, anyhow};
use perry_abi::layout::descriptor::{OMITTED_BOOL, OMITTED_REF, UNDEFINED_BOOL};
use perry_mir::ErrorKind;
use pliron::{
    builtin::ops::ModuleOp,
    context::Context,
    op::Op,
    operation::verify_operation,
    opts::{dce::DCEPass, simplify_cfg::SimplifyCFGPass},
    pass::{AnalysisManager, PMConfig, Pass, Passes},
    printable::Printable,
};
use pliron_wasm::emit::{DataSegment, EmitOptions, Memory, emit_module};

mod aggregates;
mod analysis;
mod async_split;
pub mod boundary;
mod builder;
mod dates;
mod exceptions;
mod function;
mod host;
mod intrinsics;
mod json;
mod lease;
mod memory;
mod module;
mod numeric;
mod ops;
mod optional;
mod regex;
mod repr;
mod runtime;
mod strings;
mod temporal;
mod values;
mod variants;

pub use memory::explain_tiers;

/// Structured boundary errors emitted during Wasm lowering.
#[derive(Debug)]
pub enum CodegenError {
    AsyncInitializerRequiresAsyncExport {
        core_name: String,
        span: Option<perry_mir::SourceSpan>,
    },
    MissingErrorChannel {
        core_name: String,
        span: Option<perry_mir::SourceSpan>,
    },
    ExportBinding {
        core_name: String,
        span: Option<perry_mir::SourceSpan>,
        source: anyhow::Error,
    },
}

impl std::fmt::Display for CodegenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AsyncInitializerRequiresAsyncExport { core_name, .. } => {
                write!(f, "{core_name} requires asynchronous module initialization")
            }
            Self::MissingErrorChannel { core_name, .. } => {
                write!(
                    f,
                    "{core_name} can throw without a compatible WIT error channel"
                )
            }
            Self::ExportBinding { core_name, .. } => {
                write!(f, "binding WIT export {core_name}")
            }
        }
    }
}

impl std::error::Error for CodegenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ExportBinding { source, .. } => Some(source.as_ref()),
            Self::AsyncInitializerRequiresAsyncExport { .. } | Self::MissingErrorChannel { .. } => {
                None
            }
        }
    }
}

/// Import module of runtime and helper functions, resolved by `perry-wit`'s linker.
pub const HELPER_MODULE: &str = "__perry_helper";
/// Starting address of static data, leaving address 0 as the null reference (`undefined`).
const DATA_BASE: u32 = 16;
/// The interface `setTimeout` waits on.
const MONOTONIC_CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.0";

/// How functions are exported.
pub enum Exports {
    /// Export every exported TypeScript function under its own name, with the core signature of its
    /// MIR type plus the exception status, as one task per call. An async function's export
    /// runs through settled awaits and returns its value and status, while pending host suspension
    /// uses component mode.
    Raw,
    /// Raw exports plus test-only GC stress controls and ownership counters.
    RawChecked,
    /// Canonical-ABI exports of a WIT world, and the interfaces the world imports.
    Canonical {
        /// Test-only forced collection and reclamation poisoning at existing safe points.
        memory_checks: bool,
        exports: Vec<boundary::Export>,
        imports: Vec<String>,
        bindings: Vec<boundary::Import>,
        resources: Vec<boundary::ResourceDefinition>,
    },
}

/// Lower a canonicalized MIR module and encode it as a core Wasm module that still imports
/// runtime functions from `HELPER_MODULE`.
pub fn compile(
    ctx: &mut Context,
    mir: &ModuleOp,
    exports: &Exports,
    analyses: &mut AnalysisManager,
) -> Result<Vec<u8>> {
    let (wasm, data, memory) = module::lower(ctx, mir, exports, analyses)?;
    let mut cleanup = Passes::default();
    cleanup.add_pass(SimplifyCFGPass);
    cleanup.add_pass(DCEPass);
    let mut lowered_analyses = AnalysisManager::default();
    lowered_analyses.set_config(PMConfig {
        verify_after_all: true,
        ..PMConfig::default()
    });
    while cleanup
        .run(wasm.get_operation(), ctx, &mut lowered_analyses)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?
        .ir_changed
        == pliron::irbuild::IRStatus::Changed
    {}
    // Resuming an `await` inside a loop enters the loop at a second block.
    for function in analysis::module_functions(ctx, &wasm) {
        pliron_wasm::reducible::make_reducible(ctx, &function);
    }
    verify_operation(wasm.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))
        .context("verifying the lowered wasm module")?;
    let end = DATA_BASE as usize + data.len();
    let options = EmitOptions {
        memory: if memory {
            Memory::Define {
                min_pages: end.div_ceil(65536).max(1) as u64,
                export: Some("memory".into()),
            }
        } else {
            Memory::None
        },
        data: if data.is_empty() {
            vec![]
        } else {
            vec![DataSegment {
                offset: DATA_BASE,
                bytes: data,
            }]
        },
        names: true,
    };
    Ok(emit_module(ctx, &wasm, &options)?)
}

/// Return the non-zero status code for an exception `kind` (0 represents normal completion).
pub fn status_code(kind: ErrorKind) -> i32 {
    match kind {
        ErrorKind::Any => 3,
        ErrorKind::TypeError => 1,
        ErrorKind::RangeError => 2,
        ErrorKind::ReferenceError => 6,
        ErrorKind::SyntaxError => 7,
        ErrorKind::Value => 4,
        ErrorKind::Exit => 5,
    }
}
