//! Executable contracts for the compiler's declared static TypeScript domains.
pub mod aggregates;
pub mod collections;
pub mod execution;
pub mod program;
pub mod registry;
pub mod runner;
pub mod transitions;

mod capabilities;
mod catalog;
mod emit;
mod fetch;
mod handler;
mod inferred_aggregates;
mod scaling;
mod strategies;
mod streams;

pub(crate) use catalog::{PASSING, deferrals, deferred, passing};
