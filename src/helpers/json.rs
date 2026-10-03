//! Allocation-free JSON parsing into caller-owned typed nodes.

#[path = "json/parse.rs"]
mod parse;
#[path = "json/serialize.rs"]
mod serialize;
#[path = "json/storage.rs"]
mod storage;

pub use parse::{measure, populate};
pub use serialize::{measure_serialized, serialize};

const NODE_BYTES: usize = 32;
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Null = 1,
    Boolean = 2,
    Number = 3,
    String = 4,
    Array = 5,
    Object = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Syntax(usize),
    UnpairedSurrogate(usize),
    Depth(usize),
    Capacity,
    InvalidGraph,
}
