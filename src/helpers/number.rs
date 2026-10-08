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

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_numbers_fit_and_round_trip() {
        for value in [
            0.0,
            -0.0,
            42.0,
            f64::MAX,
            f64::MIN,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            let mut bytes = [0u8; 40];
            let length = unsafe { super::number_format(value, bytes.as_mut_ptr()) } as usize;
            assert!(length <= bytes.len());
            let text = core::str::from_utf8(&bytes[..length]).unwrap();
            if value.is_finite() {
                assert_eq!(text.parse::<f64>().unwrap(), value);
            } else {
                assert!(matches!(text, "Infinity" | "-Infinity" | "NaN"));
            }
        }
    }
}
