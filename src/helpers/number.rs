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
    // Every i32 has an exact, non-exponential ECMAScript decimal representation.
    let integer = value as i32;
    let mut digits = [0u8; 11];
    let mut buffer = ryu_js::Buffer::new();
    let text = if f64::from(integer) == value {
        let mut at = digits.len();
        let mut rest = integer.unsigned_abs();
        loop {
            at -= 1;
            digits[at] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        if integer < 0 {
            at -= 1;
            digits[at] = b'-';
        }
        &digits[at..]
    } else {
        buffer.format(value).as_bytes()
    };
    // SAFETY: both representations fit in the caller's forty-byte buffer.
    unsafe {
        core::ptr::copy_nonoverlapping(text.as_ptr(), output, text.len());
    }
    text.len() as u32
}

#[cfg(test)]
mod tests {
    #[test]
    fn integer_fast_path_matches_ecmascript_formatter() {
        let mut expected = ryu_js::Buffer::new();
        let mut output = [0; 40];
        let mut check = |value| {
            // SAFETY: output holds the forty bytes required by number_format.
            let len = unsafe { super::number_format(value, output.as_mut_ptr()) } as usize;
            assert_eq!(
                &output[..len],
                expected.format(value).as_bytes(),
                "{value:?}"
            );
        };
        for integer in i16::MIN..=i16::MAX {
            check(f64::from(integer));
        }
        for value in [
            -0.0,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from(i32::MIN),
            f64::from(i32::MAX),
            1e-6,
            1e-7,
            1e20,
            1e21,
        ] {
            check(value);
            if value.is_finite() {
                check(value.next_up());
                check(value.next_down());
            }
        }
        let mut bits = 7u64;
        for _ in 0..10_000 {
            bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
            check(f64::from(bits as i32));
            check(f64::from_bits(bits));
        }
    }
}
