//! Dense-array methods lowered inline: callbacks of `map`/`filter`, and `indexOf`/`includes`.
use super::*;
use crate::control_flow::{ArrayOutput, Holes, Input, Span};
use perry_mir::ArrayAtOp;

mod callback;
mod index;
mod loops;
mod search;
pub(super) use search::Search;
mod transform;
#[derive(Clone, Copy)]
pub(super) enum Direction {
    Forward,
    Reverse,
}
#[derive(Clone, Copy)]
pub(super) enum FindResult {
    Value,
    Index,
}
#[derive(Clone, Copy)]
pub(super) enum Transform {
    Map,
    Filter,
    FlatMap,
}
#[derive(Clone, Copy)]
pub(super) enum Quantifier {
    Some,
    Every,
}
#[derive(Clone, Copy)]
pub(super) enum Mutation {
    InPlace,
    Copy,
}
