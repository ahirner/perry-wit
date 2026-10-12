//! perry MIR → pliron `wasm` dialect → core Wasm (`TODOs.md` P4).
//!
//! The output defines and exports linear memory when required, places string constants in data segments
//! from `DATA_BASE`, and imports runtime functions from `HELPER_MODULE`; `perry-wit`'s linker
//! resolves those against `perry-runtime` and the helper libraries. Exports are either the raw
//! functions ([Exports::Raw], for tests) or canonical-ABI component exports
//! ([Exports::Canonical], see [boundary]). Representation decisions for this first slice:
//!
//! - `f64` stays `f64`; semantic integers use `i32` through 32 bits and `i64` through 64 bits,
//!   with narrow results normalized at their wrap points. Records, arrays, strings and
//!   promises are `i32` addresses.
//! - `undefined` (R8) is the signalling-NaN bit pattern [perry_abi::UNDEFINED_BITS] for
//!   numbers, the null address for references and `2` for booleans. Wasm arithmetic only
//!   produces quiet NaNs, so no computed number can be mistaken for `undefined`;
//!   `perry.to_number` canonicalizes before arithmetic.
//! - Records are an `i32` shape id and a parse-source word followed by 8-byte field slots in
//!   declaration order; arrays and variants use the layouts in [perry_abi::layout].
//! - Unproven and repeated allocations use precise collectible storage. Bounded task storage
//!   remains available for small synchronous exports and task-owned handle tables.
//! - Strings are UTF-8 with a cached Unicode scalar count (layout in [perry_abi::layout::string]);
//!   constants are static objects in the data segment.
//! - Exceptions use the status convention: a function that may throw returns an extra `i32`
//!   status after its results (0 = normal, otherwise an [ErrorKind] code), and callers branch on
//!   it. The status reaches the export; P6.1 maps it to the WIT result.
//!
//! Module layout: `module` drives the lowering of a whole module and `function` that of one
//! function, whose ops `ops` dispatches to the topical modules; `async_split` turns async
//! functions into state machines and [boundary] binds them to the canonical ABI.
use anyhow::{Context as _, Result, anyhow};
use perry_mir::attrs::ErrorKind;
use pliron::{
    builtin::ops::ModuleOp, context::Context, op::Op, operation::verify_operation,
    pass::AnalysisManager, printable::Printable,
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
mod variants;

pub use memory::explain_tiers;

/// Import module of runtime and helper functions, resolved by `perry-wit`'s linker.
pub const HELPER_MODULE: &str = "__perry_helper";
/// First address of static data; address 0 stays null (`undefined` for references).
const DATA_BASE: u32 = 16;
/// The interface `setTimeout` waits on.
const MONOTONIC_CLOCK: &str = "wasi:clocks/monotonic-clock@0.3.0";
/// `undefined` in a `boolean | undefined` slot.
const UNDEFINED_BOOL: i32 = 2;

/// How functions are exported.
pub enum Exports {
    /// Every exported TypeScript function under its own name, with the core signature of its
    /// MIR type plus the exception status, as one task per call. An async function's export
    /// runs through settled awaits and returns its value and status; pending host suspension
    /// requires component mode.
    /// For tests and inspection.
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

/// The status code of an exception kind; 0 means no exception.
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
