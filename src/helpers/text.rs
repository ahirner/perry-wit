#![cfg_attr(target_arch = "wasm32", no_std)]

mod case_properties;
mod casing;
mod normalize;

/// Converts UTF-8 text, returning packed byte/scalar lengths, or u64::MAX on overflow.
/// With out_ptr = 0, measures without writing; zero means the unchanged input can be reused.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn str_case_convert(
    src_ptr: u32,
    src_byte_len: u32,
    out_ptr: u32,
    to_upper: u32,
) -> u64 {
    if src_byte_len == 0 {
        return 0;
    }
    // SAFETY: compiled callers supply valid UTF-8 and a separate output allocation.
    let input = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            src_ptr as *const u8,
            src_byte_len as usize,
        ))
    };
    let mut byte_len = 0u32;
    let mut scalar_len = 0u32;
    let mut original = input.chars();
    let mut unchanged = out_ptr == 0;
    for scalar in casing::mapped_scalars(input.chars(), to_upper != 0) {
        let Some(next_len) = byte_len.checked_add(scalar.len_utf8() as u32) else {
            return u64::MAX;
        };
        if out_ptr != 0 {
            let Some(destination) = out_ptr.checked_add(byte_len) else {
                return u64::MAX;
            };
            // SAFETY: measurement reserved the complete output, including this scalar.
            let output = unsafe {
                core::slice::from_raw_parts_mut(destination as *mut u8, scalar.len_utf8())
            };
            scalar.encode_utf8(output);
        }
        unchanged = unchanged && original.next() == Some(scalar);
        byte_len = next_len;
        scalar_len += 1;
    }
    if unchanged && original.next().is_none() {
        0
    } else {
        u64::from(byte_len) | (u64::from(scalar_len) << 32)
    }
}

/// Normalizes UTF-8 text (`mode` bit 0 = compose, bit 1 = compat), returning packed
/// byte/scalar lengths, or u64::MAX on overflow. With `out_ptr = 0`, measures without writing.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn str_normalize(
    src_ptr: u32,
    src_byte_len: u32,
    out_ptr: u32,
    mode: u32,
) -> u64 {
    if src_byte_len == 0 {
        return 0;
    }
    // SAFETY: compiled callers supply valid UTF-8 and a separate output allocation.
    let input = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            src_ptr as *const u8,
            src_byte_len as usize,
        ))
    };
    normalize::normalize_utf8(input, out_ptr, (mode & 1) != 0, (mode & 2) != 0)
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
