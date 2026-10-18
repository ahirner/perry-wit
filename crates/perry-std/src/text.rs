//! UTF-8 string methods indexed by Unicode scalar values.
mod normalize;
use crate::runtime::{bytes, from_static, scalars_length};
use perry_abi::number::{DecimalSyntax, ascii_digit, scan_decimal, split_sign};

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

pub(crate) struct View {
    string: *mut u8,
    pub(crate) text: &'static str,
    scalars: u64,
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
pub(crate) unsafe extern "C" fn std_string_split(string: *mut u8, separator: *mut u8) -> *mut u8 {
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
pub(crate) extern "C" fn std_string_scalar_at(string: *mut u8, position: f64) -> i32 {
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

/// Whether `needle` occurs at `start` of `haystack` (which has room for it). An inline loop:
/// slice equality would call `memcmp` at every candidate position.
fn matches_at(haystack: &[u8], start: usize, needle: &[u8]) -> bool {
    let mut index = 0;
    while index < needle.len() {
        if haystack[start + index] != needle[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// First occurrence: a scan for the needle's first byte, then a check of the rest.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some(&first) = needle.first() else {
        return Some(0);
    };
    let last = haystack.len().checked_sub(needle.len())?;
    let mut start = 0;
    while let Some(found) = byte_position(&haystack[start..=last], first) {
        if matches_at(haystack, start + found, needle) {
            return Some(start + found);
        }
        start += found + 1;
    }
    None
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

fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let Some(&first) = needle.first() else {
        return Some(haystack.len());
    };
    let last = haystack.len().checked_sub(needle.len())?;
    (0..=last)
        .rev()
        .find(|&start| haystack[start] == first && matches_at(haystack, start, needle))
}

fn position_of(view: &View, offset: usize) -> u64 {
    position(view.text, view.ascii, offset)
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
pub(crate) extern "C" fn std_string_starts_with(
    string: *mut u8,
    needle: *mut u8,
    position: f64,
) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let from = offset(&view, clamp(position, view.scalars));
    u32::from(tail(view.text, from).starts_with(needle.as_bytes()))
}

/// `s.endsWith(needle, end)` (pass `+Infinity` for a missing end).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_ends_with(string: *mut u8, needle: *mut u8, end: f64) -> u32 {
    let view = view(string);
    let needle = self::view(needle).text;
    let to = offset(&view, clamp(end, view.scalars));
    u32::from(head(view.text, to).ends_with(needle.as_bytes()))
}

/// ECMAScript WhiteSpace and LineTerminator code points.
fn space(scalar: char) -> bool {
    perry_abi::number::space(scalar)
}

#[inline]
fn ascii_space(byte: u8) -> bool {
    byte <= 0x20 && matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn trim_start_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some(&first) = bytes.first() {
        if first < 0x80 {
            if ascii_space(first) {
                bytes = &bytes[1..];
                continue;
            }
            break;
        }
        let s = unsafe { core::str::from_utf8_unchecked(bytes) };
        let Some(ch) = s.chars().next() else {
            break;
        };
        if space(ch) {
            bytes = &bytes[ch.len_utf8()..];
        } else {
            break;
        }
    }
    bytes
}

fn trim_end_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some(&last) = bytes.last() {
        if last < 0x80 {
            if ascii_space(last) {
                bytes = &bytes[..bytes.len() - 1];
                continue;
            }
            break;
        }
        let s = unsafe { core::str::from_utf8_unchecked(bytes) };
        let Some(ch) = s.chars().next_back() else {
            break;
        };
        if space(ch) {
            bytes = &bytes[..bytes.len() - ch.len_utf8()];
        } else {
            break;
        }
    }
    bytes
}

fn trim_bytes(bytes: &[u8]) -> &[u8] {
    trim_end_bytes(trim_start_bytes(bytes))
}

/// `s.trim()`.
///
/// # Safety
/// `string` is a live immutable UTF-8 string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_string_trim(string: *mut u8) -> *mut u8 {
    let text = unsafe { crate::runtime::bytes(string) };
    let trimmed = trim_bytes(text);
    if trimmed.len() == text.len() {
        return string;
    }
    let from = trimmed.as_ptr() as usize - text.as_ptr() as usize;
    unsafe { crate::runtime::string_view(string, from as u32, (from + trimmed.len()) as u32) }
}

/// `s.trimStart()`.
///
/// # Safety
/// `string` is a live immutable UTF-8 string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_string_trim_start(string: *mut u8) -> *mut u8 {
    let text = unsafe { crate::runtime::bytes(string) };
    let trimmed = trim_start_bytes(text);
    if trimmed.len() == text.len() {
        return string;
    }
    let from = text.len() - trimmed.len();
    unsafe { crate::runtime::string_view(string, from as u32, text.len() as u32) }
}

/// `s.trimEnd()`.
///
/// # Safety
/// `string` is a live immutable UTF-8 string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_string_trim_end(string: *mut u8) -> *mut u8 {
    let text = unsafe { crate::runtime::bytes(string) };
    let trimmed = trim_end_bytes(text);
    if trimmed.len() == text.len() {
        return string;
    }
    unsafe { crate::runtime::string_view(string, 0, trimmed.len() as u32) }
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
    let rem_bytes = offset(&fill_view, rem);
    let pad_bytes = full
        .saturating_mul(fill_view.text.len() as u64)
        .saturating_add(rem_bytes as u64);
    let total = pad_bytes.saturating_add(view.text.len() as u64);
    if total > u64::from(u32::MAX >> 2) {
        crate::runtime::raise(crate::runtime::RANGE_ERROR, "Invalid string length");
        return from_static("");
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

/// `s.padStart(maxLength, fillString)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_pad_start(
    string: *mut u8,
    max_length: f64,
    fill: *mut u8,
) -> *mut u8 {
    pad_string(string, max_length, fill, false)
}

/// `s.padEnd(maxLength, fillString)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_pad_end(
    string: *mut u8,
    max_length: f64,
    fill: *mut u8,
) -> *mut u8 {
    pad_string(string, max_length, fill, true)
}

fn replace_string(
    string: *mut u8,
    needle: *mut u8,
    replacement: *mut u8,
    replace_all: bool,
) -> *mut u8 {
    let text_str = view(string).text;
    let text = text_str.as_bytes();
    let pat = view(needle).text.as_bytes();
    let rep = view(replacement).text.as_bytes();
    let mut out = crate::runtime::Buffer::new();
    if pat.is_empty() {
        if !replace_all {
            crate::regex::push_substitution(&mut out, text, 0, 0, rep, &[]);
            out.extend_from_slice(text);
        } else {
            let mut pos = 0;
            crate::regex::push_substitution(&mut out, text, 0, 0, rep, &[]);
            for ch in text_str.chars() {
                let end = pos + ch.len_utf8();
                out.extend_from_slice(text.get(pos..end).unwrap_or(&[]));
                crate::regex::push_substitution(&mut out, text, end, end, rep, &[]);
                pos = end;
            }
        }
        return unsafe { out.into_string() };
    }
    let Some(first) = find(text, pat) else {
        return string;
    };
    out.extend_from_slice(text.get(..first).unwrap_or(&[]));
    crate::regex::push_substitution(&mut out, text, first, first + pat.len(), rep, &[]);
    let mut cursor = first + pat.len();
    if replace_all {
        while let Some(rel) = find(text.get(cursor..).unwrap_or(&[]), pat) {
            let start = cursor + rel;
            let end = start + pat.len();
            out.extend_from_slice(text.get(cursor..start).unwrap_or(&[]));
            crate::regex::push_substitution(&mut out, text, start, end, rep, &[]);
            cursor = end;
        }
    }
    out.extend_from_slice(text.get(cursor..).unwrap_or(&[]));
    unsafe { out.into_string() }
}

/// `s.replace(needle, replacement)` for string search.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_replace(
    string: *mut u8,
    needle: *mut u8,
    replacement: *mut u8,
) -> *mut u8 {
    replace_string(string, needle, replacement, false)
}

/// `s.replaceAll(needle, replacement)` for string search.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_replace_all(
    string: *mut u8,
    needle: *mut u8,
    replacement: *mut u8,
) -> *mut u8 {
    replace_string(string, needle, replacement, true)
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

/// `s.repeat(count)`; a negative or infinite count throws `RangeError`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_repeat(string: *mut u8, count: f64) -> *mut u8 {
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
static mut ASCII_STRINGS: [AsciiString; 128] = [AsciiString {
    len: 0,
    scalars: 0,
    data: core::ptr::null(),
    buffer: core::ptr::null(),
    owner: core::ptr::null(),
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
                let byte_ptr = (&raw const (*entry).cursor_byte).cast::<u8>();
                entry.write(AsciiString {
                    len: 1,
                    scalars: 1,
                    data: byte_ptr,
                    buffer: core::ptr::null(),
                    owner: core::ptr::null(),
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

/// `String.fromCodePoint(value)`: one Unicode scalar; surrogate code points are rejected.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_from_code_point(value: f64) -> *mut u8 {
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

/// ECMAScript `parseFloat(string)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_parse_float(string: *mut u8) -> f64 {
    let trimmed = trim_start_bytes(unsafe { crate::runtime::bytes(string) });
    let (negative, rest) = split_sign(trimmed);
    if rest.starts_with(b"Infinity") {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let Ok(literal) = scan_decimal::<true>(rest, DecimalSyntax::Prefix) else {
        return f64::NAN;
    };
    literal
        .decimal
        .finish(literal.mantissa, negative, literal.exponent)
}

/// ECMAScript `StringToNumber` (`Number(string)`).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_to_number(string: *mut u8) -> f64 {
    let trimmed = trim_bytes(unsafe { crate::runtime::bytes(string) });
    if trimmed.is_empty() {
        return 0.0;
    }
    if trimmed.len() > 2 && trimmed[0] == b'0' {
        let radix = match trimmed[1] {
            b'x' | b'X' => Some(16u32),
            b'o' | b'O' => Some(8u32),
            b'b' | b'B' => Some(2u32),
            _ => None,
        };
        if let Some(radix) = radix {
            let mut value = 0.0f64;
            for &byte in &trimmed[2..] {
                let Some(digit) = ascii_digit(byte, radix) else {
                    return f64::NAN;
                };
                value = value * f64::from(radix) + f64::from(digit);
            }
            return value;
        }
    }
    let (negative, rest) = split_sign(trimmed);
    if rest == b"Infinity" {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let Ok(literal) = scan_decimal::<true>(rest, DecimalSyntax::Prefix) else {
        return f64::NAN;
    };
    if literal.consumed != rest.len() {
        return f64::NAN;
    }
    literal
        .decimal
        .finish(literal.mantissa, negative, literal.exponent)
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
