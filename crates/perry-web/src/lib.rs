//! Fetch API values (`Headers`, constructed `Response` and `Request` objects with byte
//! bodies) and Web writers over the console outputs (`Writable.toWeb(process.stdout)`).
//!
//! A helper library like `perry-std`, linked only into programs that use these classes. Name,
//! value, method, status text and URL rules come from the fetch helper (`src/helpers/fetch`),
//! which the network `fetch` uses as well: header names are stored lowercased, values as
//! trimmed Latin-1 bytes (ByteStrings), and `get` joins duplicates with `, `.
//!
//! Bodies are snapshots taken at construction; reading one (`bytes()`, `text()`) marks it
//! used and resolves at once, and a second read rejects with `TypeError`, as in Node.
#![cfg_attr(target_arch = "wasm32", no_std)]

pub(crate) use perry_abi::{
    Buffer, RANGE_ERROR, TYPE_ERROR, alloc, bytes, bytes_view, from_bytes, from_static,
    promise_new, reject_with, resolve, view_bytes,
};

pub(crate) mod helper {
    perry_abi::wasm_imports! { "__perry_helper";
        pub(crate) fn fetch_header_name(
            input: *const u8,
            len: u32,
            output: *mut u8,
            cap: u32,
        ) -> u32;
        pub(crate) fn fetch_header_value(
            input: *const u8,
            len: u32,
            output: *mut u8,
            cap: u32,
        ) -> u32;
        pub(crate) fn fetch_method(input: *const u8, len: u32) -> u32;
        pub(crate) fn fetch_status_text(input: *const u8, len: u32) -> u32;
        pub(crate) fn fetch_url(input: *const u8, len: u32, output: *mut u8, cap: u32) -> u32;
        pub(crate) fn fetch_url_value(input: *const u8, len: u32, output: *mut u8, cap: u32)
        -> u32;
        pub(crate) fn json_measure(input: *const u8, len: u32) -> u64;
        pub(crate) fn json_populate_measured(
            input: *const u8,
            len: u32,
            output: *mut u8,
            cap: u32,
        ) -> u64;
    }
}

mod url;

const INVALID: u32 = u32::MAX;

/// Throw `TypeError` with `parts` concatenated.
fn type_error(parts: &[&[u8]]) {
    let mut message = Buffer::new();
    for part in parts {
        message.extend_from_slice(part);
    }
    perry_abi::raise_string(TYPE_ERROR, from_bytes(message.as_slice()));
}

mod body;
mod fetch;
mod headers;
mod http_exchange;
mod readable;
mod serve;
mod writable;

/// # Safety
/// `data` must point to `len` bytes that outlive the use.
pub(crate) unsafe fn slice(data: u32, len: u32) -> &'static [u8] {
    if len == 0 {
        return &[];
    }
    // SAFETY: guaranteed by the caller.
    unsafe { core::slice::from_raw_parts(data as usize as *const u8, len as usize) }
}
