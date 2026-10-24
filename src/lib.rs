//! Perry-WIT: compiles TypeScript directly into WASI 0.3 WebAssembly components.
//!
//! Pipeline: TypeScript → specialized HIR (`perry_frontend`) → eager MIR (`perry_lower`,
//! `perry_mir`) → Wasm dialect (`perry_codegen`, `pliron_wasm`) → linked core Wasm → component.
//! Runtime libraries are built in their own workspace and embedded through `perry_rt_build`.

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
