//! Executable contracts for the compiler's declared static TypeScript domains.
pub mod execution;
pub mod fetch;
pub mod program;
pub mod registry;
pub mod runner;
pub mod scaling;
pub mod strategies;
pub mod streams;
pub mod transitions;

pub mod capabilities;

pub mod handler;

/// Contracts the compiler must pass, one id per line. The list only grows: a listed contract
/// that fails is a regression, and an unlisted contract that passes fails the campaign until it
/// is added. Unlisted failures are `pending` (`TODOs.md` P6.2).
pub const PASSING: &str = include_str!("../passing.txt");

pub fn passing(id: &str) -> bool {
    PASSING.lines().any(|line| line.trim() == id)
}
