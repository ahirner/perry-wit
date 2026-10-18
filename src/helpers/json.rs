//! Shared JSON grammar with fixed-buffer and dynamic graph destinations.

#[path = "json/parse.rs"]
mod parse;
#[path = "json/reader.rs"]
mod reader;
#[path = "json/storage.rs"]
mod storage;

pub use parse::{measure, populate};

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
}

#[cfg(target_arch = "wasm32")]
#[path = "json/pool.rs"]
mod pool;

#[cfg(target_arch = "wasm32")]
#[path = "json/cursor.rs"]
mod cursor;
