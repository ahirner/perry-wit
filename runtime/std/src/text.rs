//! UTF-8 string methods with scalar indices, except `charCodeAt`'s UTF-16 code units.
mod normalize;
pub(crate) mod pattern;
use crate::runtime::{bytes, from_static, scalars_length};
use perry_rt_sys::whitespace::{trim_bytes, trim_end_bytes, trim_start_bytes};

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

/// Resolve a relative index (`slice`, `subarray`), counting negative values from the end and clamping to `len`.
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

pub(crate) struct View {
    string: *mut u8,
    pub(crate) text: &'static str,
    pub(crate) scalars: u64,
    ascii: bool,
}

pub(crate) fn view(string: *mut u8) -> View {
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

impl View {
    pub(crate) fn offset(&self, position: u64) -> usize {
        let position = position.min(self.scalars) as u32;
        if self.ascii {
            return position as usize;
        }
        // The cursor always names a boundary in this immutable UTF-8 payload.
        let header = unsafe { &mut *self.string.cast::<perry_rt_sys::layout::string::Header>() };
        let (base, byte) = if position >= header.cursor_scalar {
            (header.cursor_scalar, header.cursor_byte)
        } else {
            (0, 0)
        };
        let rest = &self.text[byte as usize..];
        let offset = rest
            .char_indices()
            .nth((position - base) as usize)
            .map_or(self.text.len(), |(i, _)| byte as usize + i);
        header.cursor_scalar = position;
        header.cursor_byte = offset as u32;
        offset
    }

    pub(crate) fn position(&self, byte: usize) -> u32 {
        if self.ascii {
            return byte as u32;
        }
        let header = unsafe { &mut *self.string.cast::<perry_rt_sys::layout::string::Header>() };
        let (base, from) = if byte >= header.cursor_byte as usize {
            (header.cursor_scalar, header.cursor_byte as usize)
        } else {
            (0, 0)
        };
        let position = base + scalars_length(&self.text.as_bytes()[from..byte]);
        header.cursor_scalar = position;
        header.cursor_byte = byte as u32;
        position
    }
}

fn substring(view: &View, start: u64, end: u64) -> *mut u8 {
    if start >= end {
        return from_static(perry_rt_sys::static_text!(""));
    }
    let from = view.offset(start);
    let to = view.offset(end);
    unsafe { crate::runtime::string_view(view.string, from as u32, to as u32) }
}

fn literal_matches<'a>(
    text: &'a [u8],
    needle: &'a [u8],
) -> impl Iterator<Item = (usize, usize)> + 'a {
    pattern::matches(text, move |from| {
        let start = from + find(&text[from..], needle)?;
        Some((start, start + needle.len()))
    })
}

/// # Safety
/// Both arguments must be live runtime strings containing valid UTF-8.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_string_split(string: *mut u8, separator: *mut u8) -> *mut u8 {
    let view = view(string);
    let separator = unsafe { bytes(separator) };
    let capacity = if separator.is_empty() {
        view.scalars as usize
    } else {
        0
    };
    view.split(
        literal_matches(view.text.as_bytes(), separator),
        capacity,
        |_, _| [],
    )
}

/// Read a character by scalar index, returning `undefined` for invalid, fractional, or out-of-range indices.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_index(string: *mut u8, index: f64) -> *mut u8 {
    let view = view(string);
    if !(index >= 0.0 && index < view.scalars as f64 && libm::trunc(index) == index) {
        return core::ptr::null_mut();
    }
    std_string_char_at(view.string, index)
}

/// `s.charAt(position)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_char_at(string: *mut u8, position: f64) -> *mut u8 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.scalars {
        return from_static(perry_rt_sys::static_text!(""));
    }
    let position = position as u64;
    let from = view.offset(position);
    let first = view.text.as_bytes()[from];
    if first.is_ascii() {
        return ascii_character(first);
    }
    let to = view.offset(position + 1);
    unsafe { crate::runtime::string_view(view.string, from as u32, to as u32) }
}

/// The scalar at `position`, or -1 for an empty `charAt` result. Internal MIR operation.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_scalar_at(string: *mut u8, position: f64) -> i32 {
    let view = view(string);
    let position = integer(position);
    if position < 0 || position as u64 >= view.scalars {
        return -1;
    }
    view.text
        .get(view.offset(position as u64)..)
        .expect("scalar offset")
        .chars()
        .next()
        .map_or(-1, |c| c as i32)
}

/// `s.charCodeAt(position)`, indexed by UTF-16 code unit.
#[unsafe(no_mangle)]
extern "C" fn std_string_char_code_at(string: *mut u8, position: f64) -> f64 {
    let view = view(string);
    let Ok(position) = usize::try_from(integer(position)) else {
        return f64::NAN;
    };
    let unit = if view.ascii {
        view.text.as_bytes().get(position).copied().map(u16::from)
    } else {
        view.text.encode_utf16().nth(position)
    };
    unit.map_or(f64::NAN, f64::from)
}

/// The scalar at `position`, or undefined outside the string.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_code_point_at(string: *mut u8, position: f64) -> f64 {
    let scalar = std_string_scalar_at(string, position);
    if scalar < 0 {
        f64::from_bits(crate::runtime::UNDEFINED_BITS)
    } else {
        f64::from(scalar)
    }
}

/// `s.slice(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_slice(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (start, end) = (relative(start, view.scalars), relative(end, view.scalars));
    substring(&view, start, end)
}

/// `s.substring(start, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_substring(string: *mut u8, start: f64, end: f64) -> *mut u8 {
    let view = view(string);
    let (a, b) = (clamp(start, view.scalars), clamp(end, view.scalars));
    substring(&view, a.min(b), a.max(b))
}

/// `s.indexOf(needle, position)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_index_of(
    string: *mut u8,
    needle: *mut u8,
    position: f64,
) -> f64 {
    let view = view(string);
    let needle = self::view(needle).text;
    let start = clamp(position, view.scalars);
    let from = view.offset(start);
    match find(&view.text.as_bytes()[from..], needle.as_bytes()) {
        Some(found) => view.position(from + found) as f64,
        None => -1.0,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some((&last, prefix)) = needle.split_last() else {
        return Some(0);
    };
    let candidates = haystack.get(prefix.len()..)?;
    let mut from = 0;
    while let Some(offset) = byte_position(&candidates[from..], last) {
        let start = from + offset;
        if haystack[start..].starts_with(prefix) {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(haystack.len());
    }
    haystack
        .windows(needle.len())
        .rposition(|candidate| candidate == needle)
}

/// The first position of `byte`, eight bytes per step while none of them matches.
fn byte_position(bytes: &[u8], byte: u8) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let pattern = ONES * u64::from(byte);
    let mut index = 0;
    while index + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[index..index + 8].try_into().expect("eight bytes"));
        let difference = word ^ pattern;
        // Nonzero exactly when some byte of `difference` is zero, i.e. matches `byte`.
        if difference.wrapping_sub(ONES) & !difference & (ONES << 7) != 0 {
            break;
        }
        index += 8;
    }
    bytes[index..]
        .iter()
        .position(|&candidate| candidate == byte)
        .map(|found| index + found)
}

/// `s.lastIndexOf(needle, position)` (pass `+Infinity` for a missing position).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_last_index_of(
    string: *mut u8,
    needle: *mut u8,
    position: f64,
) -> f64 {
    let view = view(string);
    let needle = self::view(needle).text;
    let start = if position.is_nan() {
        view.scalars
    } else {
        clamp(position, view.scalars)
    };
    // The match may start at `start` at the latest.
    let limit = view.offset(start);
    let end = view
        .text
        .floor_char_boundary(limit.saturating_add(needle.len()));
    match rfind(&view.text.as_bytes()[..end], needle.as_bytes()) {
        Some(found) => view.position(found) as f64,
        None => -1.0,
    }
}

/// `s.startsWith(needle, position)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_starts_with(
    string: *mut u8,
    needle: *mut u8,
    position: f64,
) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let from = view.offset(clamp(position, view.scalars));
    u32::from(view.text[from..].starts_with(needle))
}

/// `s.endsWith(needle, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_ends_with(string: *mut u8, needle: *mut u8, end: f64) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let to = view.offset(clamp(end, view.scalars));
    u32::from(view.text[..to].ends_with(needle))
}

unsafe fn trim_with(string: *mut u8, f: fn(&[u8]) -> &[u8]) -> *mut u8 {
    let text = unsafe { crate::runtime::bytes(string) };
    let trimmed = f(text);
    if trimmed.len() == text.len() {
        return string;
    }
    let from = trimmed.as_ptr() as usize - text.as_ptr() as usize;
    unsafe { crate::runtime::string_view(string, from as u32, (from + trimmed.len()) as u32) }
}

macro_rules! string_wrappers {
    ($(unsafe $name:ident($($arg:ident: $ty:ty),*) => $body:expr;)*) => {$(
        #[unsafe(no_mangle)]
        pub(crate) unsafe extern "C" fn $name($($arg: $ty),*) -> *mut u8 {
            unsafe { $body }
        }
    )*};
    ($($name:ident($($arg:ident: $ty:ty),*) => $body:expr;)*) => {$(
        #[unsafe(no_mangle)]
        pub(crate) extern "C" fn $name($($arg: $ty),*) -> *mut u8 {
            $body
        }
    )*};
}

string_wrappers! {
    unsafe std_string_trim(string: *mut u8) => trim_with(string, trim_bytes);
    unsafe std_string_trim_start(string: *mut u8) => trim_with(string, trim_start_bytes);
    unsafe std_string_trim_end(string: *mut u8) => trim_with(string, trim_end_bytes);
}

fn pad_string(string: *mut u8, max_length: f64, fill: *mut u8, is_end: bool) -> *mut u8 {
    let view = view(string);
    let target = integer(max_length);
    if target <= view.scalars as i64 {
        return string;
    }
    let fill_view = self::view(fill);
    if fill_view.scalars == 0 {
        return string;
    }
    let pad_scalars = (target as u64).saturating_sub(view.scalars);
    let full = pad_scalars / fill_view.scalars;
    let rem = pad_scalars % fill_view.scalars;
    let rem_bytes = fill_view.offset(rem);
    let pad_bytes = full
        .saturating_mul(fill_view.text.len() as u64)
        .saturating_add(rem_bytes as u64);
    let total = pad_bytes.saturating_add(view.text.len() as u64);
    if total > u64::from(u32::MAX >> 2) {
        perry_rt_sys::raise_static!(crate::runtime::RANGE_ERROR, "Invalid string length");
        return from_static(perry_rt_sys::static_text!(""));
    }
    let data = crate::runtime::alloc(total as u32);
    unsafe {
        let mut cursor = data;
        if is_end {
            core::ptr::copy_nonoverlapping(view.text.as_ptr(), cursor, view.text.len());
            cursor = cursor.add(view.text.len());
        }
        let fill_bytes = fill_view.text.len();
        for _ in 0..full as usize {
            core::ptr::copy_nonoverlapping(fill_view.text.as_ptr(), cursor, fill_bytes);
            cursor = cursor.add(fill_bytes);
        }
        if rem_bytes > 0 {
            core::ptr::copy_nonoverlapping(fill_view.text.as_ptr(), cursor, rem_bytes);
            cursor = cursor.add(rem_bytes);
        }
        if !is_end {
            core::ptr::copy_nonoverlapping(view.text.as_ptr(), cursor, view.text.len());
        }
        crate::runtime::adopt(data, total as u32)
    }
}

string_wrappers! {
    std_string_pad_start(string: *mut u8, max_length: f64, fill: *mut u8) => pad_string(string, max_length, fill, false);
    std_string_pad_end(string: *mut u8, max_length: f64, fill: *mut u8) => pad_string(string, max_length, fill, true);
}

fn replace_string(
    string: *mut u8,
    needle: *mut u8,
    replacement: *mut u8,
    replace_all: bool,
) -> *mut u8 {
    let view = view(string);
    let text = view.text.as_bytes();
    let needle = unsafe { bytes(needle) };
    let rep = unsafe { bytes(replacement) };
    view.replace(
        literal_matches(text, needle),
        replace_all,
        |out, start, end| {
            crate::regex::push_substitution(out, text, start, end, rep);
        },
    )
}

string_wrappers! {
    std_string_replace(string: *mut u8, needle: *mut u8, replacement: *mut u8) => replace_string(string, needle, replacement, false);
    std_string_replace_all(string: *mut u8, needle: *mut u8, replacement: *mut u8) => replace_string(string, needle, replacement, true);
}

/// `s.at(index)`: returns null pointer (`undefined`) when out of bounds.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_at(string: *mut u8, index: f64) -> *mut u8 {
    let view = view(string);
    let mut idx = integer(index);
    if idx < 0 {
        idx = (view.scalars as i64).saturating_add(idx);
    }
    if idx < 0 || (idx as u64) >= view.scalars {
        return core::ptr::null_mut();
    }
    std_string_char_at(view.string, idx as f64)
}

/// Implement `s.repeat(count)`, throwing `RangeError` when `count` is negative or infinite.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_repeat(string: *mut u8, count: f64) -> *mut u8 {
    let view = view(string);
    let count = integer(count);
    let total = (view.text.len() as u64).saturating_mul(count.max(0) as u64);
    if count < 0 || count == i64::MAX || total > u64::from(u32::MAX >> 2) {
        perry_rt_sys::raise_static!(crate::runtime::RANGE_ERROR, "Invalid count value");
        return from_static(perry_rt_sys::static_text!(""));
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

use perry_rt_sys::layout::string::Header as AsciiString;
static mut ASCII_STRINGS: [AsciiString; 128] = [AsciiString {
    len: 0,
    scalars: 0,
    data: core::ptr::null_mut(),
    buffer: core::ptr::null_mut(),
    owner: core::ptr::null_mut(),
    offset: 0,
    cursor_scalar: 0,
    cursor_byte: 0,
}; 128];

/// Static ASCII headers used by compiled character reads.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_ascii_strings() -> *const u8 {
    unsafe {
        let base = (&raw mut ASCII_STRINGS).cast::<AsciiString>();
        if (*base).len == 0 {
            let mut index = 0usize;
            while index < 128 {
                let entry = base.add(index);
                let byte_ptr = (&raw mut (*entry).cursor_byte).cast::<u8>();
                entry.write(AsciiString {
                    len: 1,
                    scalars: 1,
                    data: byte_ptr,
                    buffer: core::ptr::null_mut(),
                    owner: core::ptr::null_mut(),
                    offset: 0,
                    cursor_scalar: 0,
                    cursor_byte: index as u32,
                });
                index += 1;
            }
        }
        base.cast()
    }
}

fn ascii_character(byte: u8) -> *mut u8 {
    unsafe {
        std_ascii_strings()
            .add((byte as usize) * core::mem::size_of::<AsciiString>())
            .cast_mut()
    }
}

/// Implement `String.fromCodePoint(value)` for a single Unicode scalar, rejecting surrogate code points.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_from_code_point(value: f64) -> *mut u8 {
    if value < 0.0 || value > 0x10ffff as f64 || value != value as u32 as f64 {
        perry_rt_sys::raise_static!(crate::runtime::RANGE_ERROR, "Invalid Unicode scalar value");
        return from_static(perry_rt_sys::static_text!(""));
    }
    let Some(scalar) = char::from_u32(value as u32) else {
        perry_rt_sys::raise_static!(
            crate::runtime::RANGE_ERROR,
            "Surrogate code points are outside UTF-8 strings"
        );
        return from_static(perry_rt_sys::static_text!(""));
    };
    if scalar.is_ascii() {
        return ascii_character(scalar as u8);
    }
    let mut bytes = [0; 4];
    crate::runtime::from_bytes(scalar.encode_utf8(&mut bytes).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::{find, rfind};

    #[test]
    fn substring_search_matches_byte_windows_at_word_boundaries() {
        let haystack = "aé🙂aaaaaaaaabaaaaaaaab\0last".as_bytes();
        for start in 0..=haystack.len() {
            for end in start..=haystack.len() {
                let slice = &haystack[start..end];
                for needle in [
                    b"".as_slice(),
                    b"a",
                    b"aaaab",
                    "🙂".as_bytes(),
                    b"last",
                    b"\0",
                    b"absent",
                ] {
                    let (first, last) = if needle.is_empty() {
                        (Some(0), Some(slice.len()))
                    } else {
                        (
                            slice.windows(needle.len()).position(|part| part == needle),
                            slice.windows(needle.len()).rposition(|part| part == needle),
                        )
                    };
                    assert_eq!(find(slice, needle), first);
                    assert_eq!(rfind(slice, needle), last);
                }
            }
        }
    }
}
