//! Executable contracts for the compiler's declared static TypeScript domains.
mod exceptions;
pub mod execution;
pub mod fetch;
pub mod program;
pub mod registry;
pub mod runner;
pub mod scaling;
pub mod strategies;
pub mod streams;
mod strings;
pub mod transitions;

pub mod capabilities;

pub mod handler;

/// Contracts the compiler must pass, one id per line. Within the supported subset, a listed contract
/// that fails is a regression, and an unlisted contract that passes fails the campaign until it
/// is added. Unlisted failures are `pending` (`TODOs.md` P6.2); explicit deviations are `exempt`.
pub const PASSING: &str = include_str!("../passing.txt");

pub fn passing(id: &str) -> bool {
    PASSING.lines().any(|line| line.trim() == id)
}

/// Deliberate subset boundaries, distinct from features still required by the migration.
pub fn accepted_deviation(id: &str) -> Option<&'static str> {
    match id {
        "ecma.promise.await_ordering" => Some(
            "Uncontended settled awaits advance inline; synthetic ECMAScript microtask FIFO ordering is outside the supported subset.",
        ),
        "ecma.json.reviver" => Some(
            "JSON.parse revivers are deferred and diagnosed at compile time; parsing has no callback traversal or source contexts.",
        ),
        "ecma.json.replacer" | "ecma.json.stringify_options" => Some(
            "JSON.stringify replacers and formatting are deferred and diagnosed at compile time; serialization is compact and callback-free.",
        ),
        "web.streams.byob_reader" | "web.fetch.bounded_byob_body" => Some(
            "These legacy BYOB contracts require buffer detachment, which is outside the supported stream profile. Default-reader delivery, cancellation and bounded memory remain required.",
        ),
        _ => None,
    }
}
