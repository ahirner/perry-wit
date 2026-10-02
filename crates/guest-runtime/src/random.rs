//! WASI Preview 2 Random implementation for Math.random and crypto APIs.

use crate::nanbox::{nanbox_pointer, nanbox_string};
use crate::state::{get_state, JsHandle};

/// Generates a pseudo-random floating point number in [0.0, 1.0) using WASI insecure random.
pub(crate) fn math_random() -> i64 {
    let val = crate::bindings::wasi::random::insecure::get_insecure_random_u64();
    // Use 53 bits of precision for standard IEEE 754 float
    let mantissa = val & ((1u64 << 53) - 1);
    let f = (mantissa as f64) / ((1u64 << 53) as f64);
    f.to_bits() as i64
}

/// Generates a cryptographically secure UUID v4 string according to RFC 4122.
pub(crate) fn crypto_random_uuid() -> i64 {
    let mut bytes = crate::bindings::wasi::random::random::get_random_bytes(16);
    if bytes.len() < 16 {
        bytes.resize(16, 0);
    }
    // Set version 4: bits 4-7 of time_hi_and_version to 0100
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // Set variant: bits 6-7 of clock_seq_hi_and_reserved to 10
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let uuid_str = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    );

    let state = get_state();
    let id = state.strings.len();
    state.strings.push(uuid_str);
    nanbox_string(id)
}

/// Fills a typed array in-place with cryptographically secure random bytes from wasi:random.
/// Returns the input array handle per Web Cryptography API specification.
pub(crate) fn crypto_fill_random(handle: i64) -> i64 {
    let state = get_state();
    if let Some(h) = state.get_handle_mut(handle) {
        match h {
            JsHandle::Uint8Array(view) => {
                let len = view.byte_length;
                if len > 0 {
                    let rand_bytes =
                        crate::bindings::wasi::random::random::get_random_bytes(len as u64);
                    for (i, &b) in rand_bytes.iter().enumerate().take(len) {
                        view.set(i, b);
                    }
                }
            }
            JsHandle::Array(arr) => {
                let len = arr.len();
                if len > 0 {
                    let rand_bytes =
                        crate::bindings::wasi::random::random::get_random_bytes(len as u64);
                    for (i, &b) in rand_bytes.iter().enumerate().take(len) {
                        arr[i] = (b as f64).to_bits() as i64;
                    }
                }
            }
            _ => {}
        }
    }
    handle
}

/// Generates a Uint8Array containing `len` cryptographically secure random bytes.
pub(crate) fn crypto_random_bytes(size_val: i64) -> i64 {
    let size_f = f64::from_bits(size_val as u64);
    let len = if size_f.is_finite() && size_f > 0.0 {
        size_f as usize
    } else {
        0
    };
    let rand_bytes = if len > 0 {
        crate::bindings::wasi::random::random::get_random_bytes(len as u64)
    } else {
        Vec::new()
    };
    let view = crate::buffer::Uint8ArrayView::from_bytes(rand_bytes);
    let state = get_state();
    let id = state.alloc_handle(JsHandle::Uint8Array(view));
    nanbox_pointer(id)
}
