//! Allocation-free JSON parsing and serialization shared by runtime values and the guest adapter.
#![no_std]

#[path = "../../../src/helpers/json.rs"]
mod implementation;

pub use implementation::*;
