//! Perry MIR: the typed middle layer between Perry HIR and Wasm (`TODOs.md`, phase P2).
//!
//! Dialect `perry` is built on [pliron]. Values of [types::RecordType] are references to
//! closed-shape records (subset rule R1); every allocation carries a memory [attrs::Tier]
//! that passes refine from `unassigned` (design decision D3).
//!
//! The control-flow ops are adapted from `pliron-llvm`, which perry-wit must not depend on:
//! that crate's default feature links `llvm-sys`, which the LLVM audit rejects.
mod aggregates;
pub mod attrs;
pub mod calls;
pub mod collections;
pub mod control;
pub mod effects;
pub mod exceptions;
mod fold;
pub mod foreign;
pub mod globals;
pub mod heap;
pub mod integers;
pub mod intrinsics;
mod lifetimes;
pub mod locals;
pub mod optional;
pub mod pipeline;
mod ranges;
pub mod regex;
pub mod scalar;
pub mod types;
mod unwraps;
pub mod variants;
pub mod views;

#[cfg(test)]
mod tests;
