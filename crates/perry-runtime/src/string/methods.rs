//! Unicode scalar indexed operations over immutable UTF-8 string spans.
use super::*;
use crate::{UNDEFINED_NUMBER, array, exception};

const MAX_BYTES: u32 = i32::MAX as u32;

fn byte_offset(text: &str, length: u32, index: u32) -> usize {
    if text.len() == length as usize {
        index as usize
    } else {
        text.char_indices()
            .nth(index as usize)
            .map_or(text.len(), |(byte, _)| byte)
    }
}
fn integer(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        libm::trunc(value)
    }
}
fn clamp(value: f64, length: u32) -> u32 {
    integer(value).max(0.0).min(length as f64) as u32
}
fn relative(value: f64, length: u32) -> u32 {
    let value = integer(value);
    clamp(
        if value < 0.0 {
            length as f64 + value
        } else {
            value
        },
        length,
    )
}
fn failure(kind: u32, message: &'static str) -> *mut u8 {
    exception::raise_error(kind, message);
    ptr::null_mut()
}

unsafe fn span(source: *mut u8, start: usize, end: usize, units: u32) -> *mut u8 {
    // SAFETY: callers supply ordered UTF-8 boundaries in a live immutable string.
    unsafe {
        let source = &*source.cast::<Str>();
        new(
            (end - start) as u32,
            units,
            source.data.add(start),
            source.buffer,
        )
    }
}

/// # Safety
/// `source` is a live string. Mode: charAt=0, at=1, indexed property=2.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_unit(source: *mut u8, index: f64, mode: u32) -> *mut u8 {
    // SAFETY: bounds and UTF-8 boundaries are checked before building a borrowed span.
    unsafe {
        let length = (*source.cast::<Str>()).scalars;
        let mut index = if mode == 2 { index } else { integer(index) };
        if mode == 1 && index < 0.0 {
            index += length as f64;
        }
        if !index.is_finite()
            || index < 0.0
            || index >= length as f64
            || libm::trunc(index) != index
        {
            return if mode == 0 { copy("") } else { ptr::null_mut() };
        }
        let text = text(source);
        let byte = byte_offset(text, length, index as u32);
        let scalar = text[byte..].chars().next().expect("in-range string index");
        span(source, byte, byte + scalar.len_utf8(), 1)
    }
}

/// # Safety
/// `source` is live; positions count Unicode scalar values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_code_point(source: *mut u8, index: f64) -> f64 {
    // SAFETY: only in-range scalar positions reach the character lookup.
    unsafe {
        let length = (*source.cast::<Str>()).scalars;
        let index = integer(index);
        if index < 0.0 || index >= length as f64 {
            return f64::from_bits(UNDEFINED_NUMBER);
        }
        let text = text(source);
        let byte = byte_offset(text, length, index as u32);
        text[byte..].chars().next().expect("in-range string index") as u32 as f64
    }
}

/// # Safety
/// `source` is live; `end` is a number or the optional-number sentinel. Mode: slice=0, substring=1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_slice(
    source: *mut u8,
    start: f64,
    end: f64,
    mode: u32,
) -> *mut u8 {
    // SAFETY: clamping bounds precedes UTF-8 boundary checks; empty results need no boundary.
    unsafe {
        let length = (*source.cast::<Str>()).scalars;
        let bound = if mode == 0 { relative } else { clamp };
        let mut start = bound(start, length);
        let mut end = if end.to_bits() == UNDEFINED_NUMBER {
            length
        } else {
            bound(end, length)
        };
        if mode == 1 && start > end {
            core::mem::swap(&mut start, &mut end);
        }
        if start >= end {
            return copy("");
        }
        let text = text(source);
        span(
            source,
            byte_offset(text, length, start),
            byte_offset(text, length, end),
            end - start,
        )
    }
}

/// # Safety
/// Strings are live. Position is an optional number. Mode: indexOf=0,lastIndexOf=1,startsWith=2,endsWith=3.
/// Prefix/suffix modes return a matching start position, or -1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_search(
    source: *mut u8,
    needle: *mut u8,
    index: f64,
    mode: u32,
) -> f64 {
    // SAFETY: byte boundaries are derived from the strings' valid UTF-8 and cached lengths.
    unsafe {
        let length = (*source.cast::<Str>()).scalars;
        let needle_length = (*needle.cast::<Str>()).scalars;
        let index = if (mode == 1 && index.is_nan())
            || (mode == 3 && index.to_bits() == UNDEFINED_NUMBER)
        {
            length
        } else {
            clamp(index, length)
        };
        if needle_length == 0 {
            return index as f64;
        }
        let needle = text(needle);
        let text = text(source);
        let matched = match mode {
            0 => {
                let begin = byte_offset(text, length, index);
                text[begin..].find(needle).map(|offset| begin + offset)
            }
            1 => {
                let end = byte_offset(
                    text,
                    length,
                    index.saturating_add(needle_length).min(length),
                );
                text[..end].rfind(needle)
            }
            2 => {
                let begin = byte_offset(text, length, index);
                text[begin..].starts_with(needle).then_some(begin)
            }
            3 => {
                let end = byte_offset(text, length, index);
                text[..end].ends_with(needle).then(|| end - needle.len())
            }
            _ => unreachable!("verified string search mode"),
        };
        matched.map_or(-1.0, |byte| scalar_length(&text.as_bytes()[..byte]) as f64)
    }
}

fn whitespace(c: char) -> bool {
    matches!(c,'\u{0009}'..='\u{000d}'|' '| '\u{00a0}'|'\u{1680}'|'\u{2000}'..='\u{200a}'|'\u{2028}'|'\u{2029}'|'\u{202f}'|'\u{205f}'|'\u{3000}'|'\u{feff}')
}

/// # Safety
/// `source` is live. Mode: trim=0,trimStart=1,trimEnd=2.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_trim(source: *mut u8, mode: u32) -> *mut u8 {
    // SAFETY: standard string trimming returns an ordered UTF-8 subslice.
    unsafe {
        let original = text(source);
        let trimmed = match mode {
            0 => original.trim_matches(whitespace),
            1 => original.trim_start_matches(whitespace),
            _ => original.trim_end_matches(whitespace),
        };
        let start = trimmed.as_ptr() as usize - original.as_ptr() as usize;
        span(
            source,
            start,
            start + trimmed.len(),
            scalar_length(trimmed.as_bytes()),
        )
    }
}

/// # Safety
/// `source` is live; upper is 0 or 1.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_case(source: *mut u8, upper: u32) -> *mut u8 {
    // SAFETY: both passes visit immutable Unicode scalars; output is exactly measured.
    unsafe {
        let text = text(source);
        let mut bytes = 0u32;
        let mut units = 0u32;
        for scalar in super::casing::mapped_scalars(text.chars(), upper != 0) {
            let Some(length) = bytes
                .checked_add(scalar.len_utf8() as u32)
                .filter(|&length| length <= MAX_BYTES)
            else {
                return failure(exception::RANGE_ERROR, "string result is too large");
            };
            bytes = length;
            units += 1;
        }
        let data = perry_alloc(bytes);
        let mut offset = 0;
        for scalar in super::casing::mapped_scalars(text.chars(), upper != 0) {
            let mut encoded = [0; 4];
            let value = scalar.encode_utf8(&mut encoded);
            ptr::copy_nonoverlapping(value.as_ptr(), data.add(offset), value.len());
            offset += value.len();
        }
        new(bytes, units, data, ptr::null_mut())
    }
}

/// # Safety
/// `source` is live; count is a defined number.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_repeat(source: *mut u8, count: f64) -> *mut u8 {
    // SAFETY: checked product bounds allocation and every repeated copy.
    unsafe {
        let count = integer(count);
        if count < 0.0 || count.is_infinite() {
            return failure(exception::RANGE_ERROR, "invalid string repetition count");
        }
        let string = &*source.cast::<Str>();
        let bytes = count * string.len as f64;
        if bytes > MAX_BYTES as f64 {
            return failure(exception::RANGE_ERROR, "string result is too large");
        }
        if bytes == 0.0 {
            return copy("");
        }
        let data = perry_alloc(bytes as u32);
        for index in 0..count as usize {
            ptr::copy_nonoverlapping(
                string.data,
                data.add(index * string.len as usize),
                string.len as usize,
            );
        }
        new(
            bytes as u32,
            string.scalars * count as u32,
            data,
            ptr::null_mut(),
        )
    }
}

/// # Safety
/// `source` and non-null separator are live. Limit is an optional number.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_string_split(
    source: *mut u8,
    separator: *mut u8,
    limit: f64,
) -> *mut u8 {
    // SAFETY: all output spans are immutable UTF-8 substrings retained by the task.
    unsafe {
        let limit = if limit.to_bits() == UNDEFINED_NUMBER {
            u32::MAX
        } else if !limit.is_finite() {
            0
        } else {
            libm::fmod(
                libm::fmod(libm::trunc(limit), 4294967296.0) + 4294967296.0,
                4294967296.0,
            ) as u32
        };
        let result = array::perry_array_new(0);
        if limit == 0 {
            return result;
        }
        if separator.is_null() {
            array::set_slot(result, 0, source as usize as u64);
            return result;
        }
        let text = text(source);
        let separator = super::text(separator);
        if separator.is_empty() {
            for (index, (byte, scalar)) in text.char_indices().take(limit as usize).enumerate() {
                let value = span(source, byte, byte + scalar.len_utf8(), 1);
                array::set_slot(result, index, value as usize as u64);
            }
        } else {
            let mut start = 0;
            for (index, part) in text.split(separator).take(limit as usize).enumerate() {
                let value = span(
                    source,
                    start,
                    start + part.len(),
                    scalar_length(part.as_bytes()),
                );
                array::set_slot(result, index, value as usize as u64);
                start += part.len() + separator.len();
            }
        }
        result
    }
}
