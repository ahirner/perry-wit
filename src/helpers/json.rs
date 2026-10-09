//! Allocation-free JSON parsing into caller-owned typed nodes.

#[path = "json/parse.rs"]
mod parse;
#[path = "json/serialize.rs"]
mod serialize;
#[path = "json/storage.rs"]
mod storage;

pub use parse::{measure, populate};
pub use serialize::{
    Graph, StringifyOptions, Tree, Value, measure_serialized, measure_tree,
    measure_tree_with_options, serialize, serialize_tree, serialize_tree_with_options,
};

const NODE_BYTES: usize = 40;
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
    PrototypeLookup,
    Cycle,
}
