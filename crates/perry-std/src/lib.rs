//! Built-ins of compiled programs beyond the core runtime: ECMAScript string methods,
//! `Uint8Array` with `TextEncoder`/`TextDecoder`, `JSON` and randomness.
//!
//! A helper library like the others (position-independent, imported memory): `src/link`
//! links it only into modules whose code calls one of its `std_*` exports, and keeps only
//! the reachable functions. It builds on the core runtime through `__perry_helper` imports
//! ([runtime]), so a program that uses none of it carries none of its code or data.
#![cfg_attr(target_arch = "wasm32", no_std)]

pub mod bytes;
pub mod json;
pub mod random;
pub mod runtime;
pub mod text;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// `JSON.parse` of text the JSON helper rejected.
#[unsafe(no_mangle)]
pub extern "C" fn std_raise_syntax() -> u32 {
    runtime::raise(runtime::ERROR, "SyntaxError: Unexpected token in JSON");
    runtime::ERROR
}

/// `toISOString` of an invalid date.
#[unsafe(no_mangle)]
pub extern "C" fn std_raise_invalid_date() -> u32 {
    runtime::raise(runtime::RANGE_ERROR, "Invalid time value");
    runtime::RANGE_ERROR
}
