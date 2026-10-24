//! `Uint8Array`, `TextEncoder`, and `TextDecoder`.
//!
//! Views share a detachable owner, subarrays preserve offsets, and transfer reuses the payload.
use crate::{
    runtime::{ARRAY_DATA, RANGE_ERROR, alloc, bytes as string_bytes, raise},
    text::{integer, relative},
};
use core::ptr;

use perry_rt_sys::Bytes;

/// Largest supported typed-array length.
const MAX: u64 = 1 << 30;

fn zeroed(len: u32) -> *mut u8 {
    let data = alloc(len);
    // SAFETY: fresh block of `len` bytes.
    unsafe { ptr::write_bytes(data, 0, len as usize) };
    perry_rt_sys::bytes_view(len, data)
}

fn slice(bytes: *mut u8) -> &'static mut [u8] {
    // SAFETY: compiled code passes live views; only the attached range is mutable.
    unsafe {
        let view = &*bytes.cast::<Bytes>();
        if view.len == 0 || view.detached() {
            return &mut [];
        }
        core::slice::from_raw_parts_mut(view.data, view.len as usize)
    }
}

fn attached(bytes: *mut u8) -> bool {
    if unsafe { (*bytes.cast::<Bytes>()).detached() } {
        raise(
            crate::runtime::TYPE_ERROR,
            perry_rt_sys::static_text!("Cannot use a detached ArrayBuffer"),
        );
        false
    } else {
        true
    }
}

/// The canonical boundary reads refreshed bounds after observing detachment.
///
/// # Safety
/// `view` is a live byte view.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_boundary(view: *mut u8) -> *mut u8 {
    unsafe { perry_rt_sys::normalize_bytes(view) }
}

/// `bytes.buffer` retains the buffer identity even after detachment.
///
/// # Safety
/// `view` is a live byte view.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_buffer(view: *mut u8) -> *mut u8 {
    unsafe { (*view.cast::<Bytes>()).owner.cast() }
}

/// `bytes.byteOffset` is zero after detachment.
///
/// # Safety
/// `view` is a live byte view.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_offset(view: *mut u8) -> f64 {
    unsafe {
        let view = &*view.cast::<Bytes>();
        if view.detached() {
            0.0
        } else {
            f64::from(view.offset)
        }
    }
}

/// `buffer.byteLength`.
///
/// # Safety
/// `buffer` is a live buffer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_buffer_length(buffer: *mut u8) -> f64 {
    unsafe { f64::from((*buffer.cast::<perry_rt_sys::ByteBuffer>()).len) }
}

/// `new Uint8Array(buffer)` shares an attached buffer without copying its payload.
///
/// # Safety
/// `buffer` is a live buffer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_from_buffer(buffer: *mut u8) -> *mut u8 {
    let buffer = buffer.cast::<perry_rt_sys::ByteBuffer>();
    unsafe {
        if (*buffer).detached {
            raise(
                crate::runtime::TYPE_ERROR,
                perry_rt_sys::static_text!("Cannot use a detached ArrayBuffer"),
            );
            return core::ptr::null_mut();
        }
        perry_rt_sys::buffer_view(buffer, 0, (*buffer).len)
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
pub(crate) extern "C" fn std_bytes_new(length: f64) -> *mut u8 {
    let length = integer(length);
    if !(0..=MAX as i64).contains(&length) {
        raise(
            RANGE_ERROR,
            perry_rt_sys::static_text!("Invalid typed array length"),
        );
        return zeroed(0);
    }
    zeroed(length as u32)
}

/// `new Uint8Array(numbers)`.
///
/// # Safety
/// `array` must be an array of numbers.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_from_array(array: *mut u8) -> *mut u8 {
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
pub(crate) extern "C" fn std_bytes_copy(bytes: *mut u8) -> *mut u8 {
    if !attached(bytes) {
        return core::ptr::null_mut();
    }
    let source = slice(bytes);
    let copy = zeroed(source.len() as u32);
    slice(copy).copy_from_slice(source);
    copy
}

/// `bytes[index]`: the bits of a number, or of `undefined` (an `f64` sNaN payload).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bytes_get(bytes: *mut u8, position: f64) -> u64 {
    let bytes = slice(bytes);
    match index(position, bytes.len()) {
        Some(i) => f64::from(bytes[i]).to_bits(),
        None => crate::runtime::UNDEFINED_BITS,
    }
}

/// Write `bytes[index] = value`, ignoring out-of-bounds indices per typed-array semantics.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bytes_set(bytes: *mut u8, position: f64, value: f64) {
    let bytes = slice(bytes);
    if let Some(i) = index(position, bytes.len()) {
        bytes[i] = to_uint8(value);
    }
}

/// `bytes.length` and `bytes.byteLength`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bytes_length(bytes: *mut u8) -> f64 {
    slice(bytes).len() as f64
}

fn range(bytes: *mut u8, start: f64, end: f64) -> (usize, usize) {
    let len = slice(bytes).len() as u64;
    let (start, end) = (relative(start, len), relative(end, len));
    (start as usize, end.max(start) as usize)
}

/// `bytes.subarray(start, end)`: a view sharing the bytes (pass `+Infinity` for a missing end).
///
/// # Safety
/// `bytes` is a live mutable byte view.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_subarray(
    bytes: *mut u8,
    start: f64,
    end: f64,
) -> *mut u8 {
    if !attached(bytes) {
        return core::ptr::null_mut();
    }
    let (start, end) = range(bytes, start, end);
    // SAFETY: range clamps both indices to the source length.
    unsafe { perry_rt_sys::bytes_subview(bytes, start as u32, (end - start) as u32) }
}

/// `bytes.slice(start, end)`: a copy.
///
/// # Safety
/// `bytes` is a live mutable byte view.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_slice(bytes: *mut u8, start: f64, end: f64) -> *mut u8 {
    if !attached(bytes) {
        return core::ptr::null_mut();
    }
    std_bytes_copy(unsafe { std_bytes_subarray(bytes, start, end) })
}

/// Implement `target.set(source, offset)`, throwing `RangeError` when `source` exceeds the
/// target bounds. Overlapping views copy as if through an intermediate buffer per specification.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bytes_set_from(target: *mut u8, source: *mut u8, offset: f64) {
    if !attached(target) || !attached(source) {
        return;
    }
    let offset = integer(offset);
    let target_len = unsafe { (*target.cast::<Bytes>()).len as usize };
    let source_len = unsafe { (*source.cast::<Bytes>()).len as usize };
    if offset < 0 || offset as u64 + source_len as u64 > target_len as u64 {
        raise(
            RANGE_ERROR,
            perry_rt_sys::static_text!("offset is out of bounds"),
        );
        return;
    }
    // SAFETY: validated attached ranges; raw pointers permit overlapping aliases.
    unsafe {
        ptr::copy(
            (*source.cast::<Bytes>()).data,
            (*target.cast::<Bytes>()).data.add(offset as usize),
            source_len,
        );
    }
}

/// `bytes.fill(value, start, end)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bytes_fill(bytes: *mut u8, value: f64, start: f64, end: f64) {
    if !attached(bytes) {
        return;
    }
    let (start, end) = range(bytes, start, end);
    slice(bytes)[start..end].fill(to_uint8(value));
}

/// `new TextEncoder().encode(text)`.
///
/// # Safety
/// `string` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_text_encode(string: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let text = unsafe { string_bytes(string) };
    let bytes = zeroed(text.len() as u32);
    slice(bytes).copy_from_slice(text);
    bytes
}

/// Adopt mutable canonical byte storage without copying its payload.
///
/// # Safety
/// `data` is owned storage of at least `len` initialized bytes.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bytes_adopt(len: u32, data: *mut u8) -> *mut u8 {
    perry_rt_sys::bytes_view(len, data)
}

/// Borrow existing UTF-8 storage for an outbound WIT byte list. The compiler exposes this
/// header only through its read-only byte-input type.
///
/// # Safety
/// `text` is a live Perry string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_string_byte_input(text: *mut u8) -> *mut u8 {
    let bytes = unsafe { string_bytes(text) };
    perry_rt_sys::bytes_view(bytes.len() as u32, bytes.as_ptr().cast_mut())
}
