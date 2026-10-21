//! Built-ins of compiled programs beyond the core runtime: ECMAScript string methods,
//! `Uint8Array` with `TextEncoder`/`TextDecoder`, `JSON` and randomness.
//!
//! Compiled as a position-independent helper library over imported linear memory: the linker
//! includes reachable `std_*` exports on demand and resolves core runtime calls through
//! `__perry_helper` imports ([perry_abi]).
#![cfg_attr(target_arch = "wasm32", no_std)]

mod bytes;
mod decoder;
mod exception;
mod integers;
mod json;
mod number;
mod random;
mod regex;
mod text;

use perry_abi as runtime;

/// `toISOString` of an invalid date.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_raise_invalid_date() -> u32 {
    runtime::raise(runtime::RANGE_ERROR, "Invalid time value");
    runtime::take_status()
}
