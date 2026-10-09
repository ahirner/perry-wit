//! `Uint8Array`, `TextEncoder` and `TextDecoder`.
//!
//! Layout, read inline by compiled code: `len: u32` at 0, `data: u32` at 4. A view over
//! bytes in the task region; `subarray` views share bytes with their source.
use crate::{
    runtime::{
        ARRAY_DATA, RANGE_ERROR, TYPE_ERROR, adopt, alloc, bytes as string_bytes, from_static,
        raise,
    },
    text::{integer, relative},
};
use core::ptr;

pub const LEN: u32 = 0;
pub const DATA: u32 = 4;

#[repr(C)]
struct Bytes {
    len: u32,
    data: *mut u8,
}

/// Largest length accepted, as in engines that cap typed arrays well below 4 GiB.
const MAX: u64 = 1 << 30;

fn view(len: u32, data: *mut u8) -> *mut u8 {
    let bytes = alloc(core::mem::size_of::<Bytes>() as u32) as *mut Bytes;
    // SAFETY: freshly allocated.
    unsafe { bytes.write(Bytes { len, data }) };
    bytes as *mut u8
}

fn zeroed(len: u32) -> *mut u8 {
    let data = alloc(len);
    // SAFETY: fresh block of `len` bytes.
    unsafe { ptr::write_bytes(data, 0, len as usize) };
    view(len, data)
}

fn slice(bytes: *mut u8) -> &'static mut [u8] {
    let bytes = bytes as *mut Bytes;
    // SAFETY: compiled code passes live views.
    unsafe {
        if (*bytes).len == 0 {
            return &mut [];
        }
        core::slice::from_raw_parts_mut((*bytes).data, (*bytes).len as usize)
    }
}

/// ECMAScript `ToUint8`.
fn to_uint8(value: f64) -> u8 {
    if !value.is_finite() {
        return 0;
    }
    let truncated = libm::trunc(value);
    let modulo = truncated - 256.0 * libm::floor(truncated / 256.0);
    modulo as u8
}

/// A canonical numeric index in `0..len`, if `index` is one.
fn index(index: f64, len: usize) -> Option<usize> {
    if index.is_finite() && libm::trunc(index) == index && index >= 0.0 && index < len as f64 {
        Some(index as usize)
    } else {
        None
    }
}

/// `new Uint8Array(length)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_new(length: f64) -> *mut u8 {
    let length = integer(length);
    if !(0..=MAX as i64).contains(&length) {
        raise(RANGE_ERROR, "Invalid typed array length");
        return zeroed(0);
    }
    zeroed(length as u32)
}

/// `new Uint8Array(numbers)`.
///
/// # Safety
/// `array` must be an array of numbers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_bytes_from_array(array: *mut u8) -> *mut u8 {
    // SAFETY: array layout of `perry_runtime::array`.
    let (len, data) = unsafe {
        (
            *(array as *const u32),
            *(array.add(ARRAY_DATA as usize) as *const *const f64),
        )
    };
    let bytes = zeroed(len);
    let target = slice(bytes);
    for (index, byte) in target.iter_mut().enumerate() {
        // SAFETY: `len` slots.
        *byte = to_uint8(unsafe { *data.add(index) });
    }
    bytes
}

/// `new Uint8Array(bytes)` and `bytes.slice()`: a copy.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_copy(bytes: *mut u8) -> *mut u8 {
    let source = slice(bytes);
    let copy = zeroed(source.len() as u32);
    slice(copy).copy_from_slice(source);
    copy
}

/// `bytes[index]`: the bits of a number, or of `undefined` (an `f64` sNaN payload).
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_get(bytes: *mut u8, position: f64) -> u64 {
    let bytes = slice(bytes);
    match index(position, bytes.len()) {
        Some(i) => f64::from(bytes[i]).to_bits(),
        None => crate::runtime::UNDEFINED_BITS,
    }
}

/// `bytes[index] = value`; ignored outside the view, as for typed arrays.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_set(bytes: *mut u8, position: f64, value: f64) {
    let bytes = slice(bytes);
    if let Some(i) = index(position, bytes.len()) {
        bytes[i] = to_uint8(value);
    }
}

/// `bytes.length` and `bytes.byteLength`.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_length(bytes: *mut u8) -> f64 {
    slice(bytes).len() as f64
}

fn range(bytes: *mut u8, start: f64, end: f64) -> (usize, usize) {
    let len = slice(bytes).len() as u64;
    let (start, end) = (relative(start, len), relative(end, len));
    (start as usize, end.max(start) as usize)
}

/// `bytes.subarray(start, end)`: a view sharing the bytes (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_subarray(bytes: *mut u8, start: f64, end: f64) -> *mut u8 {
    let (start, end) = range(bytes, start, end);
    let data = slice(bytes).as_mut_ptr();
    // SAFETY: `start <= len`.
    view((end - start) as u32, unsafe { data.add(start) })
}

/// `bytes.slice(start, end)`: a copy.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_slice(bytes: *mut u8, start: f64, end: f64) -> *mut u8 {
    std_bytes_copy(std_bytes_subarray(bytes, start, end))
}

/// `target.set(source, offset)`; throws `RangeError` when `source` does not fit. Overlapping
/// views copy as if through a temporary, as the specification requires.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_set_from(target: *mut u8, source: *mut u8, offset: f64) {
    let offset = integer(offset);
    let (target, source) = (slice(target), slice(source));
    if offset < 0 || offset as u64 + source.len() as u64 > target.len() as u64 {
        raise(RANGE_ERROR, "offset is out of bounds");
        return;
    }
    // SAFETY: both ranges are in bounds; `copy` handles overlap.
    unsafe {
        ptr::copy(
            source.as_ptr(),
            target.as_mut_ptr().add(offset as usize),
            source.len(),
        )
    };
}

/// `bytes.fill(value, start, end)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_bytes_fill(bytes: *mut u8, value: f64, start: f64, end: f64) {
    let (start, end) = range(bytes, start, end);
    slice(bytes)[start..end].fill(to_uint8(value));
}

/// `new TextEncoder().encode(text)`.
///
/// # Safety
/// `string` must be a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_text_encode(string: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let text = unsafe { string_bytes(string) };
    let bytes = zeroed(text.len() as u32);
    slice(bytes).copy_from_slice(text);
    bytes
}

/// `new TextDecoder('utf-8', { fatal }).decode(bytes)`: strips a leading BOM; malformed input
/// throws `TypeError` when `fatal`, otherwise each maximal invalid subpart becomes U+FFFD.
#[unsafe(no_mangle)]
pub extern "C" fn std_text_decode(bytes: *mut u8, fatal: u32) -> *mut u8 {
    let mut input: &[u8] = slice(bytes);
    if input.starts_with(&[0xEF, 0xBB, 0xBF]) {
        input = &input[3..];
    }
    if let Ok(text) = core::str::from_utf8(input) {
        return crate::runtime::from_bytes(text.as_bytes());
    }
    if fatal != 0 {
        raise(
            TYPE_ERROR,
            "The encoded data was not valid for encoding utf-8",
        );
        return from_static("");
    }
    let mut size = 0;
    for chunk in input.utf8_chunks() {
        size += chunk.valid().len() + if chunk.invalid().is_empty() { 0 } else { 3 };
    }
    let data = alloc(size as u32);
    let mut at = 0;
    for chunk in input.utf8_chunks() {
        let mut put = |bytes: &[u8]| {
            // SAFETY: `size` bytes were reserved for exactly these pieces.
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), data.add(at), bytes.len()) };
            at += bytes.len();
        };
        put(chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            put("\u{FFFD}".as_bytes());
        }
    }
    // SAFETY: valid UTF-8 by construction.
    unsafe { adopt(data, size as u32) }
}
