//! UTF-8 string methods indexed by Unicode scalar values.
use crate::runtime::{bytes, from_static, scalars_length};

/// ECMAScript `ToIntegerOrInfinity`, saturated to `i64`.
pub(crate) fn integer(value: f64) -> i64 {
    if value.is_nan() {
        0
    } else if value >= 9.0e18 {
        i64::MAX
    } else if value <= -9.0e18 {
        i64::MIN
    } else {
        value as i64
    }
}

/// A relative index (`slice`, `subarray`): negative counts from the end; clamped to `len`.
pub(crate) fn relative(value: f64, len: u64) -> u64 {
    let value = integer(value);
    if value < 0 {
        (len as i64).saturating_add(value).max(0) as u64
    } else {
        (value as u64).min(len)
    }
}

fn clamp(value: f64, len: u64) -> u64 {
    integer(value).clamp(0, len as i64) as u64
}

fn offset(view: &View, position: u64) -> usize {
    // SAFETY: view holds a live string and the runtime clamps the scalar position.
    unsafe {
        crate::runtime::string_offset(view.string, position.min(u32::MAX as u64) as u32) as usize
    }
}

/// Scalar position of byte offset `offset` (a scalar boundary).
fn position(text: &str, ascii: bool, offset: usize) -> u64 {
    if ascii {
        offset as u64
    } else {
        u64::from(scalars_length(&text.as_bytes()[..offset]))
    }
}

struct View {
    string: *mut u8,
    text: &'static str,
    scalars: u64,
    ascii: bool,
}

fn view(string: *mut u8) -> View {
    // SAFETY: compiled code passes strings of valid UTF-8.
    let bytes = unsafe { bytes(string) };
    let text = unsafe { core::str::from_utf8_unchecked(bytes) };
    let scalars = u64::from(unsafe { *(string.add(4) as *const u32) });
    View {
        string,
        text,
        scalars,
        ascii: scalars == text.len() as u64,
    }
}

fn substring(view: &View, start: u64, end: u64) -> *mut u8 {
    if start >= end {
        return from_static("");
    }
    let from = offset(view, start);
    let to = offset(view, end);
    unsafe { crate::runtime::string_view(view.string, from as u32, to as u32) }
}

/// Split into immutable views, retaining the source payload through each view.
///
/// # Safety
/// Both arguments must be live runtime strings containing valid UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_string_split(string: *mut u8, separator: *mut u8) -> *mut u8 {
    let view = view(string);
    let separator = self::view(separator).text;
    let text = view.text;
    let chunks = || {
        let mut start = 0;
        let mut finished = false;
        core::iter::from_fn(move || {
            if finished {
                return None;
            }
            let end = if separator.is_empty() {
                if start == text.len() {
                    return None;
                }
                start
                    + text
                        .get(start..)
                        .unwrap_or("")
                        .chars()
                        .next()
                        .expect("scalar")
                        .len_utf8()
            } else if let Some(found) = find(tail(text, start), separator.as_bytes()) {
                start + found
            } else {
                finished = true;
                text.len()
            };
            let range = (start, end);
            start = end + separator.len();
            Some(range)
        })
    };
    let (array, slots) = crate::runtime::array_storage(chunks().count());
    for (slot, (start, end)) in slots.iter_mut().zip(chunks()) {
        let piece = if start == 0 && end == text.len() {
            string
        } else if end - start == 1 && text.as_bytes()[start].is_ascii() {
            ascii_character(text.as_bytes()[start])
        } else {
            // SAFETY: valid separators and scalar iteration produce source byte boundaries.
            unsafe { crate::runtime::string_view(string, start as u32, end as u32) }
        };
        *slot = piece as u64;
    }
    array
}

/// Scalar index access; invalid, fractional and out-of-range indices yield undefined.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_index(string: *mut u8, index: f64) -> *mut u8 {
    let view = view(string);
    if !(index >= 0.0 && index < view.scalars as f64 && libm::trunc(index) == index) {
        return core::ptr::null_mut();
    }
    std_string_char_at(view.string, index)
}

/// `s.charAt(position)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_char_at(string: *mut u8, position: f64) -> *mut u8 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.scalars {
        return from_static("");
    }
    let position = position as u64;
    let from = offset(&view, position);
    let first = view.text.as_bytes()[from];
    if first.is_ascii() {
        return ascii_character(first);
    }
    let to = offset(&view, position + 1);
    unsafe { crate::runtime::string_view(view.string, from as u32, to as u32) }
}

/// The scalar at `position`, or -1 for an empty `charAt` result. Internal MIR operation.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_scalar_at(string: *mut u8, position: f64) -> i32 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.scalars {
        return -1;
    }
    view.text
        .get(offset(&view, position as u64)..)
        .expect("scalar offset")
        .chars()
        .next()
        .map_or(-1, |c| c as i32)
}

/// The scalar at `position`, or undefined outside the string.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_code_point_at(string: *mut u8, position: f64) -> f64 {
    let scalar = std_string_scalar_at(string, position);
    if scalar < 0 {
        f64::from_bits(crate::runtime::UNDEFINED_BITS)
    } else {
        f64::from(scalar)
    }
}

/// `s.slice(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_slice(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (start, end) = (relative(start, view.scalars), relative(end, view.scalars));
    substring(&view, start, end)
}

/// `s.substring(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_substring(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (a, b) = (clamp(start, view.scalars), clamp(end, view.scalars));
    substring(&view, a.min(b), a.max(b))
}

/// `s.indexOf(needle, position)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_index_of(string: *mut u8, needle: *mut u8, position: f64) -> f64 {
    let view = view(string);
    let needle = self::view(needle).text;
    let start = clamp(position, view.scalars);
    let from = offset(&view, start);
    match find(tail(view.text, from), needle.as_bytes()) {
        Some(found) => position_of(&view, from + found) as f64,
        None => -1.0,
    }
}

/// The bytes of `text` from `from`, and up to `to`. Searching bytes is exact for UTF-8: a
/// match of a valid needle starts at a scalar boundary. Byte searches avoid `core`'s
/// two-way `str` searcher, which is larger and links a panic path the linker reads as
/// recursion (keeping the conservative stack reservation).
fn tail(text: &str, from: usize) -> &[u8] {
    text.as_bytes().get(from..).unwrap_or(&[])
}

fn head(text: &str, to: usize) -> &[u8] {
    text.as_bytes().get(..to).unwrap_or(&[])
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if let [byte] = needle {
        return haystack.iter().position(|candidate| candidate == byte);
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    if let [byte] = needle {
        return haystack.iter().rposition(|candidate| candidate == byte);
    }
    haystack
        .windows(needle.len())
        .rposition(|window| window == needle)
}

fn position_of(view: &View, offset: usize) -> u64 {
    position(view.text, view.ascii, offset)
}

/// `s.lastIndexOf(needle, position)` (pass `+Infinity` for a missing position).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_last_index_of(string: *mut u8, needle: *mut u8, position: f64) -> f64 {
    let view = view(string);
    let needle = self::view(needle).text;
    let start = if position.is_nan() {
        view.scalars
    } else {
        clamp(position, view.scalars)
    };
    // The match may start at `start` at the latest.
    let limit = offset(&view, start);
    let end = (limit + needle.len()).min(view.text.len());
    let mut end = end;
    while !view.text.is_char_boundary(end) {
        end -= 1;
    }
    match rfind(head(view.text, end), needle.as_bytes()) {
        Some(found) => position_of(&view, found) as f64,
        None => -1.0,
    }
}

/// `s.startsWith(needle, position)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_starts_with(string: *mut u8, needle: *mut u8, position: f64) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let from = offset(&view, clamp(position, view.scalars));
    u32::from(tail(view.text, from).starts_with(needle.as_bytes()))
}

/// `s.endsWith(needle, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_ends_with(string: *mut u8, needle: *mut u8, end: f64) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let to = offset(&view, clamp(end, view.scalars));
    u32::from(head(view.text, to).ends_with(needle.as_bytes()))
}

/// ECMAScript WhiteSpace and LineTerminator code points.
fn space(scalar: char) -> bool {
    matches!(
        scalar,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `s.trim()`.
///
/// # Safety
/// `string` is a live immutable UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_string_trim(string: *mut u8) -> *mut u8 {
    let view = view(string);
    let trimmed = view.text.trim_matches(space);
    if trimmed.len() == view.text.len() {
        return string;
    }
    let from = trimmed.as_ptr() as usize - view.text.as_ptr() as usize;
    unsafe { crate::runtime::string_view(string, from as u32, (from + trimmed.len()) as u32) }
}

/// `s.repeat(count)`; a negative or infinite count throws `RangeError`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_repeat(string: *mut u8, count: f64) -> *mut u8 {
    let view = view(string);
    let count = integer(count);
    let total = (view.text.len() as u64).saturating_mul(count.max(0) as u64);
    if count < 0 || count == i64::MAX || total > u64::from(u32::MAX >> 2) {
        crate::runtime::raise(crate::runtime::RANGE_ERROR, "Invalid count value");
        return from_static("");
    }
    let data = crate::runtime::alloc(total as u32);
    for index in 0..count as usize {
        // SAFETY: `data` holds `count` copies.
        unsafe {
            core::ptr::copy_nonoverlapping(
                view.text.as_ptr(),
                data.add(index * view.text.len()),
                view.text.len(),
            )
        };
    }
    // SAFETY: repeated valid UTF-8.
    unsafe { crate::runtime::adopt(data, total as u32) }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AsciiString {
    len: u32,
    scalars: u32,
    data: *const u8,
    buffer: *const u8,
    owner: *const u8,
    offset: u32,
    cursor_scalar: u32,
    cursor_byte: u32,
}
struct AsciiStrings([AsciiString; 128]);
// SAFETY: ASCII strings never update a position cursor or their immutable payload.
unsafe impl Sync for AsciiStrings {}
const ASCII: [u8; 128] = {
    let mut bytes = [0; 128];
    let mut index = 0;
    while index < 128 {
        bytes[index] = index as u8;
        index += 1;
    }
    bytes
};
static ASCII_BYTES: [u8; 128] = ASCII;
static ASCII_STRINGS: AsciiStrings = {
    let mut strings = [AsciiString {
        len: 1,
        scalars: 1,
        data: core::ptr::null(),
        buffer: core::ptr::null(),
        owner: core::ptr::null(),
        offset: 0,
        cursor_scalar: 0,
        cursor_byte: 0,
    }; 128];
    let mut index = 0;
    while index < 128 {
        // SAFETY: all positions name bytes of the static ASCII table.
        strings[index].data = unsafe { ASCII_BYTES.as_ptr().add(index) };
        index += 1;
    }
    AsciiStrings(strings)
};
fn ascii_character(byte: u8) -> *mut u8 {
    (&raw const ASCII_STRINGS.0[byte as usize])
        .cast_mut()
        .cast()
}

/// `String.fromCodePoint(value)`: one Unicode scalar; surrogate code points are rejected.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_from_code_point(value: f64) -> *mut u8 {
    if value < 0.0 || value > 0x10ffff as f64 || value != value as u32 as f64 {
        crate::runtime::raise(crate::runtime::RANGE_ERROR, "Invalid Unicode scalar value");
        return from_static("");
    }
    let Some(scalar) = char::from_u32(value as u32) else {
        crate::runtime::raise(
            crate::runtime::RANGE_ERROR,
            "Surrogate code points are outside UTF-8 strings",
        );
        return from_static("");
    };
    if scalar.is_ascii() {
        return ascii_character(scalar as u8);
    }
    let mut bytes = [0; 4];
    crate::runtime::from_bytes(scalar.encode_utf8(&mut bytes).as_bytes())
}
