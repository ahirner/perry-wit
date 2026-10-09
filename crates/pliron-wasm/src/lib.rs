//! A WebAssembly dialect for [pliron] and an emitter to core Wasm binaries.
//!
//! The dialect `wasm` keeps values in SSA form and control flow as an unstructured CFG with
//! block arguments, so ordinary pliron passes (`mem2reg`, DCE, CFG simplification, SCCP) run
//! on it unchanged. [emit::emit_module] then:
//!
//! 1. checks that every function's CFG is reducible ([reducible::make_reducible] makes it so
//!    beforehand);
//! 2. structures it with the dominator-tree algorithm of Ramsey, *Beyond Relooper: Recursive
//!    Translation of Unstructured Control Flow to Structured Control Flow* (ICFP 2022);
//! 3. schedules single-use values onto the operand stack and gives the rest locals;
//! 4. encodes the module with [`wasm_encoder`].
//!
//! Types are pliron builtins: signless `i32`/`i64` integers and `fp32`/`fp64` floats.
//! Constants are `builtin.constant`; functions are `builtin.func` inside a `builtin.module`.
//! Exports are marked on functions with [ops::set_export].
pub mod attrs;
pub mod control;
pub mod emit;
pub mod ops;
pub mod reducible;
pub mod types;

#[cfg(test)]
mod tests;
