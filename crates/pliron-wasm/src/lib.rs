//! A WebAssembly dialect for [pliron] and an emitter to core Wasm binaries.
//!
//! The dialect `wasm` keeps values in SSA form and control flow as an unstructured CFG with
//! block arguments, so ordinary pliron passes (`mem2reg`, DCE, CFG simplification, SCCP) run
//! on it unchanged. [emit::emit_module] then performs four steps:
//!
//! 1. Verify that every function's CFG is reducible after [reducible::make_reducible].
//! 2. Structure the CFG with the dominator-tree algorithm of Ramsey, *Beyond Relooper: Recursive
//!    Translation of Unstructured Control Flow to Structured Control Flow* (ICFP 2022).
//! 3. Schedule single-use values onto the operand stack and allocate locals for the remaining values.
//! 4. Encode the module with [`wasm_encoder`].
//!
//! Types are pliron builtins: signless `i32`/`i64` integers and `fp32`/`fp64` floats.
//! Constants are `builtin.constant`, and functions are `builtin.func` inside a `builtin.module`.
//! Exports are marked on functions with [ops::set_export].
pub mod attrs;
pub mod control;
pub mod emit;
pub mod ops;
pub mod reducible;
mod types;

#[cfg(test)]
mod tests;
