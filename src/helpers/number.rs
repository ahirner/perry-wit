/// Floating-point remainder preserves dividend-signed zero without rounded
/// quotient/product cancellation.
#[unsafe(no_mangle)]
pub extern "C" fn number_remainder(dividend: f64, divisor: f64) -> f64 {
    dividend % divisor
}

/// Writes the ECMAScript representation into the caller's forty-byte buffer.
///
/// # Safety
/// `output` must point to at least forty writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn number_format(value: f64, output: *mut u8) -> u32 {
    let mut buffer = ryu_js::Buffer::new();
    let text = buffer.format(value);
    unsafe {
        core::ptr::copy_nonoverlapping(text.as_ptr(), output, text.len());
    }
    text.len() as u32
}
