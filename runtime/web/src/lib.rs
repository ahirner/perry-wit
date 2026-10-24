//! Fetch API values (`Headers`, constructed `Response` and `Request` objects with byte
//! bodies) and Web writers over the console outputs (`Writable.toWeb(process.stdout)`).
//!
//! Linked on demand into programs that use these classes. Name,
//! value, method, status text, and URL validation rules come from the fetch helper
//! shared with network `fetch`: header names are stored lowercased, values are stored as
//! trimmed Latin-1 bytes (`ByteString`), and `get` joins duplicate values with `, `.
//!
//! Bodies are snapshots captured at construction. Reading a body (`bytes()`, `text()`) marks it
//! used and resolves immediately, and a second read rejects with `TypeError` as in Node.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

pub(crate) use perry_rt_sys::{
    Buffer, RANGE_ERROR, TYPE_ERROR, alloc, bytes, bytes_view, from_bytes, from_static,
    promise_new, reject_with, resolve, view_bytes,
};

pub(crate) mod helper {
    perry_rt_sys::wasm_imports! { "__perry_helper";
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

pub(crate) fn join_bytes(parts: &[&[u8]]) -> *mut u8 {
    let mut message = Buffer::new();
    for part in parts {
        message.extend_from_slice(part);
    }
    from_bytes(message.as_slice())
}

/// Throw `TypeError` with `parts` concatenated.
fn type_error(parts: &[&[u8]]) {
    perry_rt_sys::raise_string(TYPE_ERROR, join_bytes(parts));
}

pub(crate) unsafe fn word(area: *const u8, offset: usize) -> u32 {
    unsafe { *(area.add(offset) as *const u32) }
}

macro_rules! unlink_node {
    ($head:expr, $target:expr) => {{
        let mut link: *mut _ = $head;
        while !(*link).is_null() {
            if *link == $target {
                *link = (**link).next;
                break;
            }
            link = &mut (**link).next;
        }
    }};
}
pub(crate) use unlink_node;

pub(crate) unsafe fn validate_request_init(
    url: *mut u8,
    method: *mut u8,
    has_body: bool,
    text: u32,
    h: *mut u8,
) -> Result<(*mut u8, u32), *mut u8> {
    let input = unsafe { bytes(url) };
    let capacity = 32 + input.len() as u32 * 3 + 16;
    let output = alloc(capacity);
    if unsafe { helper::fetch_url(input.as_ptr(), input.len() as u32, output, capacity) } != 0 {
        return Err(join_bytes(&[b"Failed to parse URL from ", input]));
    }
    let name = unsafe { bytes(method) };
    let tag = unsafe { helper::fetch_method(name.as_ptr(), name.len() as u32) };
    if tag == INVALID {
        return Err(join_bytes(&[b"'", name, b"' is not a valid HTTP method."]));
    }
    if has_body && matches!(tag, 0 | 1) {
        return Err(from_static(
            "Request with GET/HEAD method cannot have body.",
        ));
    }
    if text != 0 {
        headers::set_default(h, b"content-type", b"text/plain;charset=UTF-8");
    }
    Ok((output, tag))
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
