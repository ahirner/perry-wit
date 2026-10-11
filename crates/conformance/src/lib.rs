//! Executable contracts for the compiler's declared static TypeScript domains.
pub mod execution;
pub mod program;
pub mod registry;
pub mod runner;
pub mod transitions;

mod capabilities;
mod fetch;
mod handler;
mod scaling;
mod strategies;
mod streams;

/// Contracts the compiler must pass, one id per line. The list only grows: a listed contract
/// that fails is a regression, and an unlisted contract that passes fails the campaign until it
/// is added. Unlisted failures are `pending`.
const PASSING: &str = include_str!("../passing.txt");

pub(crate) fn passing(id: &str) -> bool {
    PASSING.lines().any(|line| line.trim() == id)
}

/// Contracts outside the accepted domain, each with the deviation it records: not run, and
/// neither passing nor pending.
const DEFERRED: &str = include_str!("../deferred.txt");

/// `(id, reason)` of each deferred contract.
pub(crate) fn deferrals() -> impl Iterator<Item = (&'static str, &'static str)> {
    DEFERRED
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| match line.split_once('#') {
            Some((id, reason)) => (id.trim(), reason.trim()),
            None => (line, ""),
        })
}

/// The deviation a deferred contract records.
pub(crate) fn deferred(id: &str) -> Option<&'static str> {
    deferrals()
        .find(|(deferred, _)| *deferred == id)
        .map(|(_, reason)| reason)
}
