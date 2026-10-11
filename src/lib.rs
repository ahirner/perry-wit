//! Perry-WIT: compiles TypeScript directly into WASI 0.3 WebAssembly components.
//!
//! Pipeline: TypeScript → Perry HIR → perry MIR (pliron, `crates/perry-mir`, built by
//! `crates/perry-lower`) → `wasm` dialect (`crates/perry-codegen`, `crates/pliron-wasm`) →
//! core Wasm → linked with `perry-runtime` and the helper libraries ([link]) → component.

mod abi;
mod audit;
pub mod compiler;
pub mod component;
mod link;
pub mod sdk;
mod strip;

pub use compiler::{
    CompileOptions, Compiled, compile_core, compile_file, compile_for_world, compile_typescript,
};
pub use sdk::{SdkOptions, SdkResult, generate_sdk_files};
