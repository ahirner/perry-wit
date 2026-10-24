//! Shared JSON grammar with fixed-buffer and dynamic graph destinations.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

mod parse;
mod reader;
mod storage;

pub use parse::{measure, populate};

const NODE_BYTES: usize = 32;
const MAX_DEPTH: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
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
mod pool;

#[cfg(target_arch = "wasm32")]
mod cursor;

#[cfg(target_arch = "wasm32")]
mod guest;
