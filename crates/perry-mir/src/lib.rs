//! Perry MIR: the typed middle layer between Perry HIR and Wasm (`TODOs.md`, phase P2).
//!
//! Dialect `perry` is built on [pliron]. Values of [types::RecordType] are references to
//! closed-shape records (subset rule R1); every allocation carries a memory [attrs::Tier]
//! that passes refine from `unassigned` (design decision D3).
//!
//! The control-flow ops are adapted from `pliron-llvm`, which perry-wit must not depend on:
//! that crate's default feature links `llvm-sys`, which the LLVM audit rejects.
pub mod attrs;
pub mod calls;
pub mod collections;
pub mod control;
pub mod dictionaries;
pub mod exceptions;
pub mod heap;
pub mod intrinsics;
pub mod json;
pub mod locals;
pub mod optional;
pub mod pipeline;
pub mod scalar;
pub mod types;

#[cfg(test)]
mod tests;
