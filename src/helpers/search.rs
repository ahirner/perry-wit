#![no_std]

/// Finds the first matching scalar position of needle in haystack starting at or after start_scalar.
/// Returns the matching scalar index as f64, or -1.0 if not found.
#[unsafe(no_mangle)]
pub extern "C" fn str_find_substring(
    haystack_ptr: u32,
    haystack_byte_len: u32,
    needle_ptr: u32,
    needle_byte_len: u32,
    start_scalar: u32,
) -> f64 {
    if needle_byte_len == 0 {
        let total_scalars = count_scalars(haystack_ptr, haystack_byte_len);
        if start_scalar > total_scalars {
            return total_scalars as f64;
        }
        return start_scalar as f64;
    }

    if needle_byte_len > haystack_byte_len {
        return -1.0;
    }

    let h_ptr = haystack_ptr as *const u8;
    let n_ptr = needle_ptr as *const u8;
    let h_len = haystack_byte_len as usize;
    let n_len = needle_byte_len as usize;

    let mut current_scalar: u32 = 0;
    let mut byte_offset: usize = 0;

    while byte_offset < h_len {
        if current_scalar >= start_scalar {
            if h_len - byte_offset >= n_len {
                let mut matches = true;
                let mut i = 0;
                while i < n_len {
                    if unsafe { *h_ptr.add(byte_offset + i) != *n_ptr.add(i) } {
                        matches = false;
                        break;
                    }
                    i += 1;
                }
                if matches {
                    return current_scalar as f64;
                }
            }
        }

        let first = unsafe { *h_ptr.add(byte_offset) };
        let width = match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        byte_offset += width;
        current_scalar += 1;
    }

    -1.0
}

/// Translates a scalar offset to a byte offset within valid UTF-8 bytes.
/// Clamps to the total byte length if target_scalar exceeds the string's scalar count.
#[unsafe(no_mangle)]
pub extern "C" fn str_scalar_to_byte(
    haystack_ptr: u32,
    haystack_byte_len: u32,
    target_scalar: u32,
) -> u32 {
    if haystack_byte_len == 0 || target_scalar == 0 {
        return 0;
    }
    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;
    let mut current_scalar: u32 = 0;
    let mut byte_offset: usize = 0;
    while current_scalar < target_scalar && byte_offset < h_len {
        let first = unsafe { *h_ptr.add(byte_offset) };
        let width = match first {
            0x00..=0x7F => 1,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        byte_offset += width;
        current_scalar += 1;
    }
    byte_offset as u32
}

fn count_scalars(haystack_ptr: u32, haystack_byte_len: u32) -> u32 {
    if haystack_byte_len == 0 {
        return 0;
    }
    let h_ptr = haystack_ptr as *const u8;
    let h_len = haystack_byte_len as usize;
    let mut count = 0;
    let mut offset = 0;
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
