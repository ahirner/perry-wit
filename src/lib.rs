//! Perry-WIT: compiles TypeScript directly into WASI 0.3 WebAssembly components.
//!
//! Pipeline: TypeScript → Perry HIR → perry MIR (pliron, `crates/perry-mir`, built by
//! `crates/perry-lower`) → `wasm` dialect (`crates/perry-codegen`, `crates/pliron-wasm`) →
//! core Wasm → linked with `perry-runtime` and the helper libraries → component.

mod abi;
pub mod api;
mod audit;
pub mod compiler;
pub use api::compile as compile_with_diagnostics;
pub use diagnostics::{
    CompileError, DiagnosticCode, DiagnosticSeverity, PerryDiagnostic, SourceSpan,
};
pub use perry_wit_diagnostics as diagnostics;
pub mod component;
mod link;
pub mod sdk;
mod strip;

pub use compiler::{
    CompileOptions, Compiled, ComponentOutput, compile_core, compile_file, compile_for_world,
    compile_typescript,
};
pub use sdk::{SdkOptions, SdkResult, generate_sdk_files};
