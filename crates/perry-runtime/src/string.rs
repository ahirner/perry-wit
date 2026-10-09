//! Strings: UTF-8 bytes with their UTF-16 length cached (design decision, `TODOs.md` P6.1).
//!
//! WIT strings are UTF-8, so arguments and results cross the component boundary without
//! transcoding. ECMAScript lengths and indices count UTF-16 code units; the cached count makes
//! `.length` O(1), and `utf16 == len` identifies ASCII strings, where every index is a byte
//! offset.
//!
//! Layout, read inline by compiled code: `len: u32` (bytes) at 0, `utf16: u32` at 4,
//! `data: u32` at 8, `buffer: u32` at 12. Compiled constants are static objects with a null
//! buffer. Concatenation writes into a growable buffer shared by successive results:
//! `s = s + x` appends in place while `s` is still the buffer's most recent string, so append
//! loops are linear, and every earlier string keeps its own length over the shared prefix.
use crate::region::perry_alloc;
use core::ptr;

pub const LEN: u32 = 0;
pub const UTF16: u32 = 4;
pub const DATA: u32 = 8;
pub const BUFFER: u32 = 12;
pub const SIZE: u32 = 16;

#[repr(C)]
struct Str {
    len: u32,
    utf16: u32,
    data: *mut u8,
    buffer: *mut Buffer,
}

/// Header of a concatenation buffer; `cap` bytes follow.
#[repr(C)]
struct Buffer {
    cap: u32,
    used: u32,
}

const MIN_BUFFER: u32 = 32;

#[cfg(target_arch = "wasm32")]
const _: () = assert!(core::mem::size_of::<Str>() == SIZE as usize);

/// UTF-16 code units in valid UTF-8: one per scalar, two for scalars above U+FFFF.
pub fn utf16_length(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|&b| u32::from(b & 0xC0 != 0x80) + u32::from(b >= 0xF0))
        .sum()
}

fn new(len: u32, utf16: u32, data: *mut u8, buffer: *mut Buffer) -> *mut u8 {
    let string = perry_alloc(core::mem::size_of::<Str>() as u32) as *mut Str;
    // SAFETY: freshly allocated, 8-aligned and SIZE bytes long.
    unsafe {
        string.write(Str {
            len,
            utf16,
            data,
            buffer,
        })
    };
    string as *mut u8
}

/// A string over `len` bytes of valid UTF-8 at `data`, which must outlive the task.
///
/// # Safety
/// `data` must point to `len` readable bytes of valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let bytes = if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(data, len as usize) }
    };
    let data = if len == 0 {
        ptr::NonNull::<u8>::dangling().as_ptr()
    } else {
        data
    };
    new(len, utf16_length(bytes), data, ptr::null_mut())
}

/// Strict equality, including optional strings represented by null.
///
/// # Safety
/// Each non-null operand is a live string reference.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_equal(a: *mut u8, b: *mut u8) -> u32 {
    if a.is_null() || b.is_null() {
        return u32::from(a == b);
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        let (a, b) = (&*(a as *const Str), &*(b as *const Str));
        u32::from(
            core::slice::from_raw_parts(a.data, a.len as usize)
                == core::slice::from_raw_parts(b.data, b.len as usize),
        )
    }
}

/// `a + b`.
///
/// # Safety
/// Both arguments must be strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_concat(a: *mut u8, b: *mut u8) -> *mut u8 {
    let (a, b) = (a as *mut Str, b as *mut Str);
    // SAFETY: the caller passes live strings; buffers hold `cap` bytes after their header.
    unsafe {
        if (*b).len == 0 {
            return a as *mut u8;
        }
        if (*a).len == 0 {
            return b as *mut u8;
        }
        let len = (*a).len.checked_add((*b).len).expect("string too long");
        let utf16 = (*a).utf16 + (*b).utf16;
        let buffer = (*a).buffer;
        if !buffer.is_null() {
            let bytes = (buffer as *mut u8).add(8);
            let at_end = (*a).data.add((*a).len as usize) == bytes.add((*buffer).used as usize);
            if at_end && (*buffer).cap - (*buffer).used >= (*b).len {
                ptr::copy_nonoverlapping(
                    (*b).data,
                    bytes.add((*buffer).used as usize),
                    (*b).len as usize,
                );
                (*buffer).used += (*b).len;
                return new(len, utf16, (*a).data, buffer);
            }
        }
        let cap = len.saturating_mul(2).max(MIN_BUFFER);
        let buffer = perry_alloc(8 + cap) as *mut Buffer;
        buffer.write(Buffer { cap, used: len });
        let bytes = (buffer as *mut u8).add(8);
        ptr::copy_nonoverlapping((*a).data, bytes, (*a).len as usize);
        ptr::copy_nonoverlapping((*b).data, bytes.add((*a).len as usize), (*b).len as usize);
        new(len, utf16, bytes, buffer)
    }
}

/// # Safety
/// `string` must be live for the entire returned borrow.
#[cfg(target_arch = "wasm32")]
pub(crate) unsafe fn bytes<'a>(string: *mut u8) -> &'a [u8] {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let string = &*(string as *const Str);
        if string.len == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(string.data, string.len as usize)
        }
    }
}
