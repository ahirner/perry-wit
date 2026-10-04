//! Perry-WIT: Compiles TypeScript directly into WASI 0.3 WebAssembly components.
#![warn(unreachable_pub)]

#[cfg(test)]
extern crate self as perry_wit;

pub mod abi;
pub mod compiler;
pub mod component;
pub mod conformance;
pub mod sdk;
pub mod strip;
pub mod waffle_backend;

pub use compiler::{CompileOptions, Compiled, compile_file, compile_typescript};
pub use sdk::{SdkOptions, SdkResult, generate_sdk_files};
pub use waffle_backend::{
    WaffleCompileOptions, WaffleCompiled, compile_typescript as compile_typescript_waffle,
};
