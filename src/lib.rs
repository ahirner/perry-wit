//! Perry-WIT: Compiles TypeScript directly into WASI Preview 2 WebAssembly components.
#![warn(unreachable_pub)]

pub mod abi;
pub mod compiler;
pub mod component;
pub mod conformance;
pub mod linker;
pub mod runtime;
pub mod sdk;
pub mod strip;

pub use compiler::{
    CompileOptions, Compiled, compile_file, compile_typescript, compile_typescript_raw,
};
pub use sdk::{SdkOptions, SdkResult, generate_sdk_files};
