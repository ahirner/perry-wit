#![no_std]

mod case_properties;
mod casing;

/// Returns scalar code point as f64, or f64::NAN if out-of-range or invalid position.
#[unsafe(no_mangle)]
pub extern "C" fn str_code_point_at(
    haystack_ptr: u32,
    haystack_byte_len: u32,
    position: f64,
) -> f64 {
    // Rust's saturating cast maps NaN and (-1, 0) to zero, as ToIntegerOrInfinity requires.
    if position <= -1.0 || position >= (u32::MAX as f64) + 1.0 {
        return f64::NAN;
    }
    let target_scalar = position as u32;
    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;

    let mut current_scalar = 0u32;
    let mut byte_offset = 0usize;

    while byte_offset < h_len {
        let b0 = unsafe { *h_ptr.add(byte_offset) };
        let (width, code_point) = match b0 {
            0x00..=0x7F => (1, b0 as u32),
            0xC0..=0xDF => {
                if byte_offset + 1 >= h_len {
                    return f64::NAN;
                }
                let b1 = unsafe { *h_ptr.add(byte_offset + 1) };
                (2, (((b0 & 0x1F) as u32) << 6) | ((b1 & 0x3F) as u32))
            }
            0xE0..=0xEF => {
                if byte_offset + 2 >= h_len {
                    return f64::NAN;
                }
                let b1 = unsafe { *h_ptr.add(byte_offset + 1) };
                let b2 = unsafe { *h_ptr.add(byte_offset + 2) };
                (
                    3,
                    (((b0 & 0x0F) as u32) << 12)
                        | (((b1 & 0x3F) as u32) << 6)
                        | ((b2 & 0x3F) as u32),
                )
            }
            0xF0..=0xF7 => {
                if byte_offset + 3 >= h_len {
                    return f64::NAN;
                }
                let b1 = unsafe { *h_ptr.add(byte_offset + 1) };
                let b2 = unsafe { *h_ptr.add(byte_offset + 2) };
                let b3 = unsafe { *h_ptr.add(byte_offset + 3) };
                (
                    4,
                    (((b0 & 0x07) as u32) << 18)
                        | (((b1 & 0x3F) as u32) << 12)
                        | (((b2 & 0x3F) as u32) << 6)
                        | ((b3 & 0x3F) as u32),
                )
            }
            _ => return f64::NAN,
        };

        if current_scalar == target_scalar {
            return code_point as f64;
        }

        byte_offset += width;
        current_scalar += 1;
    }

    f64::NAN
}

/// Validates code_point and writes UTF-8 bytes to out_ptr.
/// Returns byte length (1..=4), or u32::MAX (0xFFFFFFFF) if invalid / surrogate / out-of-range.
#[unsafe(no_mangle)]
pub extern "C" fn str_from_code_point(code_point: f64, out_ptr: u32) -> u32 {
    if code_point.is_nan()
        || code_point.is_infinite()
        || code_point < 0.0
        || code_point > 0x10FFFF as f64
    {
        return u32::MAX;
    }
    let cp = code_point as u32;
    if (code_point - cp as f64).abs() > 0.0 {
        return u32::MAX;
    }
    // Reject surrogates (0xD800..=0xDFFF)
    if cp >= 0xD800 && cp <= 0xDFFF {
        return u32::MAX;
    }

    let out = out_ptr as *mut u8;
    match cp {
        0x00..=0x7F => {
            unsafe { *out = cp as u8 };
            1
        }
        0x80..=0x7FF => {
            unsafe {
                *out = 0xC0 | ((cp >> 6) as u8);
                *out.add(1) = 0x80 | ((cp & 0x3F) as u8);
            }
            2
        }
        0x800..=0xFFFF => {
            unsafe {
                *out = 0xE0 | ((cp >> 12) as u8);
                *out.add(1) = 0x80 | (((cp >> 6) & 0x3F) as u8);
                *out.add(2) = 0x80 | ((cp & 0x3F) as u8);
            }
            3
        }
        0x10000..=0x10FFFF => {
            unsafe {
                *out = 0xF0 | ((cp >> 18) as u8);
                *out.add(1) = 0x80 | (((cp >> 12) & 0x3F) as u8);
                *out.add(2) = 0x80 | (((cp >> 6) & 0x3F) as u8);
                *out.add(3) = 0x80 | ((cp & 0x3F) as u8);
            }
            4
        }
        _ => u32::MAX,
    }
}

/// Converts UTF-8 text, returning packed byte/scalar lengths, or u64::MAX on overflow.
/// With out_ptr = 0, measures the exact allocation without writing output.
#[unsafe(no_mangle)]
pub extern "C" fn str_case_convert(
    src_ptr: u32,
    src_byte_len: u32,
    out_ptr: u32,
    to_upper: u32,
) -> u64 {
    let input = Utf8Scalars {
        pointer: src_ptr,
        remaining: src_byte_len,
    };
    let mut byte_len = 0u32;
    let mut scalar_len = 0u32;
    for scalar in casing::mapped_scalars(input, to_upper != 0) {
        let Some(next_len) = byte_len.checked_add(scalar.len_utf8() as u32) else {
            return u64::MAX;
        };
        if out_ptr != 0 {
            let Some(destination) = out_ptr.checked_add(byte_len) else {
                return u64::MAX;
            };
            str_from_code_point(scalar as u32 as f64, destination);
        }
        byte_len = next_len;
        scalar_len += 1;
    }
    u64::from(byte_len) | (u64::from(scalar_len) << 32)
}

#[derive(Clone)]
struct Utf8Scalars {
    pointer: u32,
    remaining: u32,
}

impl Iterator for Utf8Scalars {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if self.remaining == 0 {
            return None;
        }
        let code_point = str_code_point_at(self.pointer, self.remaining, 0.0);
        assert!(code_point.is_finite(), "Invalid UTF-8 in string helper");
        let scalar = char::from_u32(code_point as u32).expect("Valid Unicode scalar");
        self.pointer += scalar.len_utf8() as u32;
        self.remaining -= scalar.len_utf8() as u32;
        Some(scalar)
    }
}

/// Returns the number of chunks produced by splitting haystack by sep.
#[unsafe(no_mangle)]
pub extern "C" fn str_split_count(
    haystack_ptr: u32,
    haystack_byte_len: u32,
    sep_ptr: u32,
    sep_byte_len: u32,
) -> u32 {
    if haystack_byte_len == 0 {
        if sep_byte_len == 0 {
            return 0;
        } else {
            return 1;
        }
    }

    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;

    if sep_byte_len == 0 {
        // Split by empty string: count total Unicode scalars
        let mut count = 0u32;
        let mut offset = 0usize;
        while offset < h_len {
            let b = unsafe { *h_ptr.add(offset) };
            let width = match b {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            offset += width;
            count += 1;
        }
        return count;
    }

    let s_ptr = sep_ptr as *const u8;
    let s_len = sep_byte_len as usize;
    if s_len > h_len {
        return 1;
    }

    let mut occurrences = 0u32;
    let mut offset = 0usize;
    while offset + s_len <= h_len {
        let mut matches = true;
        let mut i = 0usize;
        while i < s_len {
            if unsafe { *h_ptr.add(offset + i) != *s_ptr.add(i) } {
                matches = false;
                break;
            }
            i += 1;
        }
        if matches {
            occurrences += 1;
            offset += s_len;
        } else {
            offset += 1;
        }
    }

    occurrences + 1
}

/// Populates out_descriptors_ptr with 12-byte string descriptors: [ptr: u32, byte_len: u32, scalar_len: u32]
/// for each split chunk. Returns total chunks written.
#[unsafe(no_mangle)]
pub extern "C" fn str_split_populate(
    haystack_ptr: u32,
    haystack_byte_len: u32,
    sep_ptr: u32,
    sep_byte_len: u32,
    out_descriptors_ptr: u32,
) -> u32 {
    let out = out_descriptors_ptr as *mut u32;

    if haystack_byte_len == 0 {
        if sep_byte_len == 0 {
            return 0;
        } else {
            // One empty string descriptor
            unsafe {
                *out = haystack_ptr;
                *out.add(1) = 0;
                *out.add(2) = 0;
            }
            return 1;
        }
    }

    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;

    if sep_byte_len == 0 {
        // Split by empty separator: each chunk is a 1-scalar string descriptor
        let mut chunk_idx = 0usize;
        let mut offset = 0usize;
        while offset < h_len {
            let b = unsafe { *h_ptr.add(offset) };
            let width = match b {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            unsafe {
                let slot = out.add(chunk_idx * 3);
                *slot = haystack_ptr + offset as u32;
                *slot.add(1) = width as u32;
                *slot.add(2) = 1;
            }
            chunk_idx += 1;
            offset += width;
        }
        return chunk_idx as u32;
    }

    let s_ptr = sep_ptr as *const u8;
    let s_len = sep_byte_len as usize;

    let mut chunk_idx = 0usize;
    let mut chunk_start = 0usize;
    let mut offset = 0usize;

    while offset + s_len <= h_len {
        let mut matches = true;
        let mut i = 0usize;
        while i < s_len {
            if unsafe { *h_ptr.add(offset + i) != *s_ptr.add(i) } {
                matches = false;
                break;
            }
            i += 1;
        }
        if matches {
            // Write descriptor for chunk [chunk_start..offset]
            let chunk_bytes = (offset - chunk_start) as u32;
            let chunk_scalars = count_scalars(haystack_ptr + chunk_start as u32, chunk_bytes);
            unsafe {
                let slot = out.add(chunk_idx * 3);
                *slot = haystack_ptr + chunk_start as u32;
                *slot.add(1) = chunk_bytes;
                *slot.add(2) = chunk_scalars;
            }
            chunk_idx += 1;
            offset += s_len;
            chunk_start = offset;
        } else {
            offset += 1;
        }
    }

    // Trailing chunk [chunk_start..h_len]
    let chunk_bytes = (h_len - chunk_start) as u32;
    let chunk_scalars = count_scalars(haystack_ptr + chunk_start as u32, chunk_bytes);
    unsafe {
        let slot = out.add(chunk_idx * 3);
        *slot = haystack_ptr + chunk_start as u32;
        *slot.add(1) = chunk_bytes;
        *slot.add(2) = chunk_scalars;
    }
    chunk_idx += 1;

    chunk_idx as u32
}

/// Returns total byte length needed to join count descriptors with sep.
#[unsafe(no_mangle)]
pub extern "C" fn str_join_total_len(descriptors_ptr: u32, count: u32, sep_byte_len: u32) -> u64 {
    if count == 0 {
        return 0;
    }
    let descs = descriptors_ptr as *const u32;
    let mut total = u64::from(count - 1) * u64::from(sep_byte_len);
    for i in 0..count {
        let chunk_byte_len = unsafe { *descs.add((i as usize) * 3 + 1) };
        let Some(next) = total.checked_add(u64::from(chunk_byte_len)) else {
            return u64::MAX;
        };
        total = next;
    }
    total
}

/// Joins count string descriptors separated by sep, writing to out_ptr.
/// Returns (total_bytes as u64) | ((total_scalars as u64) << 32).
#[unsafe(no_mangle)]
pub extern "C" fn str_join(
    descriptors_ptr: u32,
    count: u32,
    sep_ptr: u32,
    sep_byte_len: u32,
    out_ptr: u32,
) -> u64 {
    if count == 0 {
        return 0;
    }

    let descs = descriptors_ptr as *const u32;
    let sep = sep_ptr as *const u8;
    let out = out_ptr as *mut u8;

    let sep_scalars = if sep_byte_len > 0 {
        count_scalars(sep_ptr, sep_byte_len)
    } else {
        0
    };

    let mut out_offset = 0usize;
    let mut total_scalars = 0u32;

    for i in 0..count {
        if i > 0 && sep_byte_len > 0 {
            let mut s = 0usize;
            while s < sep_byte_len as usize {
                unsafe { *out.add(out_offset + s) = *sep.add(s) };
                s += 1;
            }
            out_offset += sep_byte_len as usize;
            total_scalars += sep_scalars;
        }

        let desc_slot = unsafe { descs.add((i as usize) * 3) };
        let chunk_ptr = unsafe { *desc_slot } as *const u8;
        let chunk_byte_len = unsafe { *desc_slot.add(1) } as usize;
        let chunk_scalar_len = unsafe { *desc_slot.add(2) };

        let mut b = 0usize;
        while b < chunk_byte_len {
            unsafe { *out.add(out_offset + b) = *chunk_ptr.add(b) };
            b += 1;
        }
        out_offset += chunk_byte_len;
        total_scalars += chunk_scalar_len;
    }

    (out_offset as u64) | ((total_scalars as u64) << 32)
}

fn count_scalars(haystack_ptr: u32, haystack_byte_len: u32) -> u32 {
    if haystack_byte_len == 0 {
        return 0;
    }
    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;
    let mut count = 0u32;
    let mut offset = 0usize;
    while offset < h_len {
        let first = unsafe { *h_ptr.add(offset) };
        let width = match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        offset += width;
        count += 1;
    }
    count
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
