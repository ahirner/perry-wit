#![cfg_attr(target_arch = "wasm32", no_std)]

mod casing;
#[path = "guest_memory.rs"]
mod guest_memory;

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

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
