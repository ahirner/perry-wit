//! The low word is a native reference; bit 32 distinguishes a byte view from a string.
//! Zero represents undefined only when the static type is optional. No wrapper is allocated.
use crate::{bytes, string};
pub const BYTES_TAG: u64 = 1 << 32;

/// # Safety
/// The value contains a live string or byte-view reference with its matching tag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_text_bytes_length(value: u64) -> f64 {
    let reference = value as u32 as usize as *mut u8;
    // SAFETY: the tag identifies the live native representation.
    unsafe {
        if value & BYTES_TAG == 0 {
            reference.add(string::SCALARS as usize).cast::<u32>().read() as f64
        } else {
            bytes::perry_bytes_length(reference)
        }
    }
}
/// # Safety
/// The value contains a live string or byte-view reference with its matching tag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_text_bytes_truthy(value: u64) -> u32 {
    // SAFETY: only strings need a length check; byte views are always truthy.
    u32::from(value & BYTES_TAG != 0 || unsafe { perry_text_bytes_length(value) } != 0.0)
}
/// # Safety
/// Each value is zero or contains a live string/byte-view reference with its matching tag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_text_bytes_equal(a: u64, b: u64) -> u32 {
    if a == b {
        return 1;
    }
    if a == 0 || b == 0 || a & BYTES_TAG != 0 || b & BYTES_TAG != 0 {
        return 0;
    }
    // SAFETY: distinct nonempty values without the byte tag contain live string references.
    unsafe {
        string::perry_string_equal(a as u32 as usize as *mut u8, b as u32 as usize as *mut u8)
    }
}
