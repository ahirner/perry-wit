#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

use perry_rt_sys as _;

mod casing;
use perry_rt_sys::guest_memory;

/// Converts UTF-8 text, returning packed byte/scalar lengths, or `u64::MAX` on overflow.
/// When `out_ptr` is 0, this measures the output size, and a return value of 0 indicates
/// the input is already in target form.
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
    let input = unsafe { guest_memory::str_from_raw(src_ptr, src_byte_len) };
    let mut sink = guest_memory::OutSink::new(out_ptr);
    let mut original = input.chars();
    let mut unchanged = out_ptr == 0;
    for scalar in casing::mapped_scalars(input.chars(), to_upper != 0) {
        if !sink.emit(scalar) {
            return u64::MAX;
        }
        unchanged = unchanged && original.next() == Some(scalar);
    }
    if unchanged && original.next().is_none() {
        0
    } else {
        sink.finish()
    }
}
