//! Exception model.
//!
//! Runtime failures in the supported subset are ECMAScript exceptions that `try`/`catch` can
//! observe; Wasm traps are reserved for resource exhaustion and compiler bugs, because a trap
//! aborts the component instance and cannot be caught by guest code.
//!
//! An op implementing [MayThrowInterface] may throw. Before Wasm lowering, a pass makes every
//! such op explicit: its block is split, the normal path continues, and the exceptional path
//! branches to the innermost `catch` (or returns the exception to the caller). The `(i32 status,
//! payload)` return convention or Wasm exception handling is chosen there (open question in
//! `TODOs.md`), so MIR stays independent of it.
use crate::attrs::ErrorKind;
use pliron::{context::Context, derive::op_interface, op::Op, result::Result};

#[op_interface]
pub trait MayThrowInterface {
    /// The error this op raises when it throws.
    fn exception(&self, ctx: &Context) -> ErrorKind;

    fn verify(_op: &dyn Op, _ctx: &Context) -> Result<()>
    where
        Self: Sized,
    {
        Ok(())
    }
}
