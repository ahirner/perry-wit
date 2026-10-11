//! Immutable UTF-8 strings with cached scalar counts and shared append storage.
//! Views retain an owner, byte offset and length. The cached data pointer keeps boundary
//! transfers direct; a scalar-position cursor makes sequential non-ASCII indexing linear.
use crate::region::perry_alloc;
use core::ptr;

pub const LEN: u32 = 0;
pub const SCALARS: u32 = 4;
pub const DATA: u32 = 8;
pub const BUFFER: u32 = 12;
pub const SIZE: u32 = 32;
pub const OWNER: u32 = 16;
pub const CURSOR_SCALAR: u32 = 24;
pub const CURSOR_BYTE: u32 = 28;

#[repr(C)]
pub(crate) struct Str {
    len: u32,
    scalars: u32,
    data: *mut u8,
    buffer: *mut Buffer,
    owner: *mut u8,
    offset: u32,
    cursor_scalar: u32,
    cursor_byte: u32,
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

/// Unicode scalar count in valid UTF-8.
pub fn scalars_length(bytes: &[u8]) -> u32 {
    bytes.iter().map(|&b| u32::from(b & 0xC0 != 0x80)).sum()
}

fn new(len: u32, scalars: u32, data: *mut u8, buffer: *mut Buffer) -> *mut u8 {
    static SHAPE: [u32; 3] = [0, 1, core::mem::offset_of!(Str, owner) as u32];
    // SAFETY: the only owner field retains the immutable payload allocation.
    let string = unsafe {
        crate::region::perry_alloc_shaped(core::mem::size_of::<Str>() as u32, SHAPE.as_ptr())
    } as *mut Str;
    let owner = if buffer.is_null() {
        data
    } else {
        buffer.cast()
    };
    // SAFETY: freshly allocated, 8-aligned and SIZE bytes long.
    unsafe {
        string.write(Str {
            len,
            scalars,
            data,
            buffer,
            owner,
            offset: (data as usize).wrapping_sub(owner as usize) as u32,
            cursor_scalar: 0,
            cursor_byte: 0,
        })
    };
    string as *mut u8
}

/// The UTF-8 bytes of a string.
///
/// # Safety
/// `string` must be a string that outlives the returned slice's use.
pub(crate) unsafe fn bytes(string: *mut u8) -> &'static [u8] {
    let string = string as *mut Str;
    // SAFETY: guaranteed by the caller.
    unsafe { core::slice::from_raw_parts((*string).data, (*string).len as usize) }
}

/// A string over static text.
pub(crate) fn from_static(text: &'static str) -> *mut u8 {
    new(
        text.len() as u32,
        scalars_length(text.as_bytes()),
        text.as_ptr() as *mut u8,
        ptr::null_mut(),
    )
}

/// A string over `len` bytes of valid UTF-8 at `data`, which must outlive the task.
///
/// # Safety
/// `data` must point to `len` readable bytes of valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { core::slice::from_raw_parts(data, len as usize) };
    new(len, scalars_length(bytes), data, ptr::null_mut())
}

#[inline(always)]
fn append_buffer(len: u32) -> *mut Buffer {
    assert!(len <= (u32::MAX - 8) / 2, "string too long");
    let cap = (len * 2).max(MIN_BUFFER);
    let buffer = perry_alloc(cap + 8) as *mut Buffer;
    // SAFETY: the allocation reserves the header and its capacity; no collection occurs here.
    unsafe { buffer.write(Buffer { cap, used: len }) };
    buffer
}

/// `a + b`.
///
/// # Safety
/// Both arguments must be strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_concat(a: *mut u8, b: *mut u8) -> *mut u8 {
    // SAFETY: both inputs are live; their headers remain immutable.
    unsafe {
        let (s, t) = (a as *mut Str, b as *mut Str);
        if (*t).len == 0 || (*s).len == 0 {
            return shared(if (*t).len == 0 { s } else { t });
        }
        append_text::<false>(s, t)
    }
}

/// `string` itself when its header is immutable (no buffer: constants and adopted text),
/// otherwise a copy of the header. A header with a buffer is only ever reachable from the
/// value that [perry_string_concat] or [perry_string_append] returned, which is what lets
/// [perry_string_append] update it in place.
#[inline(always)]
unsafe fn shared(string: *mut Str) -> *mut u8 {
    // SAFETY: the caller passes a live string.
    unsafe {
        if (*string).buffer.is_null() {
            return string as *mut u8;
        }
        new(
            (*string).len,
            (*string).scalars,
            (*string).data,
            (*string).buffer,
        )
    }
}

/// `a + b` where `a` is not used afterwards (compiled code proved it, `perry.string_append`):
/// a header made by concatenation is updated in place instead of allocating a new one, so a
/// loop of `text = text + x` allocates only when its buffer doubles.
///
/// # Safety
/// Both arguments must be strings. The caller owns `a`'s header exclusively and must not
/// read its old value afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_append(a: *mut u8, b: *mut u8) -> *mut u8 {
    // SAFETY: the caller owns a's header; b's payload remains readable until copying finishes.
    unsafe {
        let (s, t) = (a as *mut Str, b as *mut Str);
        if (*t).len == 0 {
            return a;
        }
        if (*s).buffer.is_null() || (*s).len == 0 {
            return perry_string_concat(a, b);
        }
        append_text::<true>(s, t)
    }
}

/// Copy immutable text into append storage. The suffix header is only borrowed, so it may
/// reside on the caller's stack. No collection occurs during the operation.
#[inline(always)]
unsafe fn append_text<const REUSE: bool>(s: *mut Str, suffix: *const Str) -> *mut u8 {
    unsafe {
        let len = (*s)
            .len
            .checked_add((*suffix).len)
            .expect("string too long");
        let scalars = (*s).scalars + (*suffix).scalars;
        let buffer = (*s).buffer;
        if !buffer.is_null() {
            let bytes = (buffer as *mut u8).add(8);
            let at_end = (*s).data.add((*s).len as usize) == bytes.add((*buffer).used as usize);
            if at_end && (*buffer).cap - (*buffer).used >= (*suffix).len {
                ptr::copy_nonoverlapping(
                    (*suffix).data,
                    bytes.add((*buffer).used as usize),
                    (*suffix).len as usize,
                );
                (*buffer).used += (*suffix).len;
                if REUSE {
                    (*s).len = len;
                    (*s).scalars += (*suffix).scalars;
                    return s.cast();
                }
                return new(len, scalars, (*s).data, buffer);
            }
        }
        let fresh = append_buffer(len);
        let bytes = (fresh as *mut u8).add(8);
        ptr::copy_nonoverlapping((*s).data, bytes, (*s).len as usize);
        ptr::copy_nonoverlapping(
            (*suffix).data,
            bytes.add((*s).len as usize),
            (*suffix).len as usize,
        );
        if REUSE && !buffer.is_null() {
            (*s).data = bytes;
            (*s).buffer = fresh;
            (*s).owner = fresh.cast();
            (*s).offset = 8;
            (*s).cursor_scalar = 0;
            (*s).cursor_byte = 0;
            (*s).len = len;
            (*s).scalars += (*suffix).scalars;
            s.cast()
        } else {
            new(len, scalars, bytes, fresh)
        }
    }
}

unsafe fn append_scalar<const REUSE: bool>(string: *mut u8, scalar: i32) -> *mut u8 {
    unsafe {
        if scalar < 0 {
            return if REUSE { string } else { shared(string.cast()) };
        }
        // MIR scalar producers establish validity before crossing this runtime boundary.
        let scalar = char::from_u32_unchecked(scalar as u32);
        let mut bytes = [0; 4];
        let len = scalar.encode_utf8(&mut bytes).len() as u32;
        let suffix = Str {
            len,
            scalars: 1,
            data: bytes.as_mut_ptr(),
            buffer: ptr::null_mut(),
            owner: ptr::null_mut(),
            offset: 0,
            cursor_scalar: 0,
            cursor_byte: 0,
        };
        append_text::<REUSE>(string.cast(), &suffix)
    }
}

/// Concatenate an unboxed MIR character without materializing a temporary string.
///
/// # Safety
/// `string` is live and `scalar` is a Unicode scalar or -1 for empty text.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_scalar_concat(string: *mut u8, scalar: i32) -> *mut u8 {
    unsafe { append_scalar::<false>(string, scalar) }
}

/// Append an unboxed MIR character to an exclusively owned string header.
///
/// # Safety
/// `string` obeys `perry_string_append`'s ownership contract; `scalar` is a scalar or -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_scalar_append(string: *mut u8, scalar: i32) -> *mut u8 {
    unsafe { append_scalar::<true>(string, scalar) }
}

/// Compare two strings in Unicode scalar order (the byte order of valid UTF-8):
/// negative, zero or positive. Null (`undefined` in `string | undefined`) equals only null.
///
/// # Safety
/// Both arguments must be strings or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_compare(a: *mut u8, b: *mut u8) -> i32 {
    if a == b {
        return 0;
    }
    if a.is_null() || b.is_null() {
        return 1;
    }
    let (a, b) = (a as *mut Str, b as *mut Str);
    // SAFETY: the caller passes live strings of valid UTF-8.
    let (a, b) = unsafe {
        (
            core::slice::from_raw_parts((*a).data, (*a).len as usize),
            core::slice::from_raw_parts((*b).data, (*b).len as usize),
        )
    };
    compare_bytes(a, b)
}

/// Compare whole words, then the earliest differing byte in little-endian memory order.
fn compare_bytes(a: &[u8], b: &[u8]) -> i32 {
    let common = a.len().min(b.len());
    let (a_words, a_tail) = a[..common].as_chunks::<8>();
    let (b_words, b_tail) = b[..common].as_chunks::<8>();
    for (&a, &b) in a_words.iter().zip(b_words) {
        let (a, b) = (u64::from_le_bytes(a), u64::from_le_bytes(b));
        if a != b {
            let shift = (a ^ b).trailing_zeros() & !7;
            return ((a >> shift) & 255) as i32 - ((b >> shift) & 255) as i32;
        }
    }
    for (&a, &b) in a_tail.iter().zip(b_tail) {
        if a != b {
            return i32::from(a) - i32::from(b);
        }
    }
    match a.len().cmp(&b.len()) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// Byte offset of a clamped scalar position, reusing the last forward scan.
///
/// # Safety
/// `string` is a live string. A position beyond its scalar length clamps to its byte length.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_offset(string: *mut u8, position: u32) -> u32 {
    // SAFETY: caller supplies the string and the cache always names a scalar boundary.
    unsafe {
        let s = &mut *(string as *mut Str);
        let position = position.min(s.scalars);
        if s.scalars == s.len {
            return position;
        }
        let (mut scalar, mut offset) = if position >= s.cursor_scalar {
            (s.cursor_scalar, s.cursor_byte)
        } else {
            (0, 0)
        };
        while scalar < position {
            let first = *s.data.add(offset as usize);
            offset += if first < 0x80 {
                1
            } else if first < 0xe0 {
                2
            } else if first < 0xf0 {
                3
            } else {
                4
            };
            scalar += 1;
        }
        s.cursor_scalar = scalar;
        s.cursor_byte = offset;
        offset
    }
}

/// Shared immutable substring over byte boundaries.
///
/// # Safety
/// `[from, to]` are ordered UTF-8 boundaries in a live string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_view(string: *mut u8, from: u32, to: u32) -> *mut u8 {
    unsafe {
        let source = &*(string as *const Str);
        let data = source.data.add(from as usize);
        let view = new(
            to - from,
            scalars_length(core::slice::from_raw_parts(data, (to - from) as usize)),
            data,
            source.buffer,
        ) as *mut Str;
        (*view).owner = source.owner;
        (*view).offset = source.offset + from;
        view.cast()
    }
}

#[cfg(test)]
mod tests {
    use super::compare_bytes;

    #[test]
    fn comparison_matches_byte_order_across_lengths_and_alignments() {
        let original = [b'a'; 80];
        for offset in 0..8 {
            for length in 0..40 {
                let a = &original[offset..offset + length];
                for other_length in 0..40 {
                    let mut storage = original;
                    let b = &mut storage[7 - offset..7 - offset + other_length];
                    assert_eq!(compare_bytes(a, b).cmp(&0), a.cmp(b));
                    for index in 0..other_length {
                        for byte in [0, 1, b'Z', b'b', 127, 128, 255] {
                            b[index] = byte;
                            assert_eq!(compare_bytes(a, b).cmp(&0), a.cmp(b));
                            assert_eq!(compare_bytes(b, a).cmp(&0), (&*b).cmp(a));
                            b[index] = b'a';
                        }
                    }
                }
            }
        }
    }
}
