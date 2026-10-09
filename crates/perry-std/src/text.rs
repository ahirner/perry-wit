//! String methods with ECMAScript (UTF-16 code unit) indices over UTF-8 strings.
//!
//! Positions convert to byte offsets by scanning, with a direct path for ASCII strings
//! (`utf16 == len`). A position inside a surrogate pair rounds to the scalar boundary before
//! it, so no lone surrogate arises. The conformance domain is BMP scalars, where a scalar is
//! one code unit and every result matches Node; surrogate halves (WTF-8) are out of scope.
use crate::runtime::{bytes, from_bytes, from_static, utf16_length};

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

/// Byte offset of UTF-16 position `position` (at most the length).
fn offset(text: &str, ascii: bool, position: u64) -> usize {
    if ascii {
        return (position as usize).min(text.len());
    }
    let mut units = 0u64;
    for (index, scalar) in text.char_indices() {
        let width = scalar.len_utf16() as u64;
        if units + width > position {
            return index;
        }
        units += width;
    }
    text.len()
}

/// UTF-16 position of byte offset `offset` (a scalar boundary).
fn position(text: &str, ascii: bool, offset: usize) -> u64 {
    if ascii {
        offset as u64
    } else {
        u64::from(utf16_length(&text.as_bytes()[..offset]))
    }
}

struct View {
    text: &'static str,
    units: u64,
    ascii: bool,
}

fn view(string: *mut u8) -> View {
    // SAFETY: compiled code passes strings of valid UTF-8.
    let bytes = unsafe { bytes(string) };
    let text = unsafe { core::str::from_utf8_unchecked(bytes) };
    let units = u64::from(utf16_length(bytes));
    View {
        text,
        units,
        ascii: units == text.len() as u64,
    }
}

fn substring(view: &View, start: u64, end: u64) -> *mut u8 {
    if start >= end {
        return from_static("");
    }
    let from = offset(view.text, view.ascii, start);
    let to = offset(view.text, view.ascii, end);
    from_bytes(&view.text.as_bytes()[from..to])
}

/// `s.charAt(position)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_char_at(string: *mut u8, position: f64) -> *mut u8 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.units {
        return from_static("");
    }
    let position = position as u64;
    if view.ascii {
        return substring(&view, position, position + 1);
    }
    let from = offset(view.text, false, position);
    let scalar = view
        .text
        .get(from..)
        .and_then(|rest| rest.chars().next())
        .unwrap_or('\0');
    if scalar.len_utf16() == 2 {
        // Half of a surrogate pair.
        return from_static("\u{FFFD}");
    }
    from_bytes(&view.text.as_bytes()[from..from + scalar.len_utf8()])
}

/// `s.charCodeAt(position)`: the UTF-16 code unit, or `NaN`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_char_code_at(string: *mut u8, position: f64) -> f64 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.units {
        return f64::NAN;
    }
    view.text
        .encode_utf16()
        .nth(position as usize)
        .map_or(f64::NAN, f64::from)
}

/// `s.slice(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_slice(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (start, end) = (relative(start, view.units), relative(end, view.units));
    substring(&view, start, end)
}

/// `s.substring(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_substring(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (a, b) = (clamp(start, view.units), clamp(end, view.units));
    substring(&view, a.min(b), a.max(b))
}

/// `s.indexOf(needle, position)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_string_index_of(string: *mut u8, needle: *mut u8, position: f64) -> f64 {
    let view = view(string);
    let needle = self::view(needle).text;
    let start = clamp(position, view.units);
    let from = offset(view.text, view.ascii, start);
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
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
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
        view.units
    } else {
        clamp(position, view.units)
    };
    // The match may start at `start` at the latest.
    let limit = offset(view.text, view.ascii, start);
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
    let from = offset(view.text, view.ascii, clamp(position, view.units));
    u32::from(tail(view.text, from).starts_with(needle.as_bytes()))
}

/// `s.endsWith(needle, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub extern "C" fn std_string_ends_with(string: *mut u8, needle: *mut u8, end: f64) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let to = offset(view.text, view.ascii, clamp(end, view.units));
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
#[unsafe(no_mangle)]
pub extern "C" fn std_string_trim(string: *mut u8) -> *mut u8 {
    let view = view(string);
    let trimmed = view.text.trim_matches(space);
    if trimmed.len() == view.text.len() {
        return string;
    }
    from_bytes(trimmed.as_bytes())
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
