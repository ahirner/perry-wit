//! Canonical ABI definitions, WIT metadata introspection, and trampoline synthesis.

pub mod export_names;
pub mod trampoline;
pub mod wit_meta;

pub use trampoline::synthesize_trampolines;
pub use wit_meta::{
    AbiType, ExportedWitFunction, WitWorldExports, extract_world_exports, matches_export_name,
    to_kebab_case,
};
