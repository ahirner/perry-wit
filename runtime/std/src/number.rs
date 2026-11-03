//! Dynamic numeric helpers (`parseInt`, `Number.prototype.toString`).

use crate::text::view;
use perry_rt_sys::layout::numeric;
use perry_rt_sys::number::{Number, parse_integer};

#[inline(always)]
fn digit_byte(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn format_integer_radix(magnitude: u64, negative: bool, radix: u32) -> *mut u8 {
    let mut digits = [0; 66];
    crate::runtime::from_bytes(perry_rt_sys::number::format_integer(
        magnitude,
        negative,
        radix,
        &mut digits,
    ))
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_to_string(tag: i32, bits: u64) -> *mut u8 {
    match tag {
        numeric::SIGNED => {
            let value = bits as i64;
            format_integer_radix(value.unsigned_abs(), value < 0, 10)
        }
        numeric::UNSIGNED => format_integer_radix(bits, false, 10),
        _ => {
            let mut buf = [0u8; 40];
            let len = crate::runtime::number_format(f64::from_bits(bits), &mut buf);
            crate::runtime::from_bytes(&buf[..len])
        }
    }
}

type BigUint = perry_rt_sys::number::Big<40>;

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_inspect(tag: i32, bits: u64) -> *mut u8 {
    if tag == numeric::FLOAT && bits == (-0.0f64).to_bits() {
        crate::runtime::from_static(perry_rt_sys::static_text!("-0"))
    } else {
        std_number_to_string(tag, bits)
    }
}

/// Unsigned significand and binary exponent of a finite float.
fn binary_magnitude(abs: f64) -> (u64, i32) {
    let bits = abs.to_bits();
    let biased_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac_bits = bits & ((1u64 << 52) - 1);
    if biased_exp == 0 {
        (frac_bits, -1074)
    } else {
        (frac_bits | (1u64 << 52), biased_exp - 1075)
    }
}

fn format_float_radix(abs: f64, neg: bool, radix: u32) -> *mut u8 {
    // Exact integer fast path strictly below 2^64 (18446744073709551616.0 == 2^64 as f64).
    if libm::trunc(abs) == abs && abs < 18446744073709551616.0 {
        return format_integer_radix(abs as u64, neg, radix);
    }
    let (mantissa, exp2) = binary_magnitude(abs);
    let even = (mantissa & 1) == 0;

    let mut out = crate::runtime::Buffer::new();
    if neg {
        out.push(b'-');
    }

    let mut int_val = if exp2 >= 0 {
        let mut b = BigUint::from_u64(mantissa);
        b.shl(exp2 as u32);
        b
    } else {
        let shift = (-exp2) as usize;
        if shift >= 64 {
            BigUint::zero()
        } else {
            BigUint::from_u64(mantissa >> shift)
        }
    };

    if int_val.is_zero() {
        out.push(b'0');
    } else {
        let mut rev_digits = [0u8; 1100];
        let mut at = rev_digits.len();
        while !int_val.is_zero() {
            at -= 1;
            let rem = int_val.div_rem_u32(radix) as u8;
            rev_digits[at] = digit_byte(rem);
        }
        out.extend_from_slice(&rev_digits[at..]);
    }

    if exp2 < 0 {
        let shift = (-exp2) as usize;
        let frac_mantissa = if shift >= 64 {
            mantissa
        } else {
            mantissa & ((1u64 << shift) - 1)
        };
        if frac_mantissa != 0 {
            out.push(b'.');
            let mut r = BigUint::from_u64(frac_mantissa);
            r.shl(1);
            let mut s = BigUint::from_u64(1);
            s.shl((shift + 1) as u32);
            let mut delta = BigUint::from_u64(1);

            loop {
                r.mul_add(radix, 0);
                delta.mul_add(radix, 0);
                let mut digit = r.div_rem_small_quot(&s) as u8;

                let low = match r.compare(&delta) {
                    core::cmp::Ordering::Less => true,
                    core::cmp::Ordering::Equal => even,
                    core::cmp::Ordering::Greater => false,
                };
                let mut sum = r.clone();
                sum.add_assign(&delta);
                let high = match sum.compare(&s) {
                    core::cmp::Ordering::Greater => true,
                    core::cmp::Ordering::Equal => even,
                    core::cmp::Ordering::Less => false,
                };

                if low || high || r.is_zero() {
                    if (high && !low)
                        || (high && low && {
                            let mut two_r = r.clone();
                            two_r.shl(1);
                            two_r.compare(&s) == core::cmp::Ordering::Greater
                        })
                    {
                        digit += 1;
                    }
                    out.push(digit_byte(digit));
                    break;
                }
                out.push(digit_byte(digit));
            }
        }
    }

    crate::runtime::from_bytes(out.as_slice())
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_to_string_radix(tag: i32, bits: u64, radix: f64) -> *mut u8 {
    let r = crate::text::integer(radix);
    if !(2..=36).contains(&r) {
        crate::runtime::raise(
            crate::runtime::RANGE_ERROR,
            perry_rt_sys::static_text!("Invalid radix"),
        );
        return crate::runtime::from_static(perry_rt_sys::static_text!(""));
    }
    let radix_u32 = r as u32;
    if radix_u32 == 10 {
        return std_number_to_string(tag, bits);
    }
    match tag {
        numeric::SIGNED => {
            let value = bits as i64;
            format_integer_radix(value.unsigned_abs(), value < 0, radix_u32)
        }
        numeric::UNSIGNED => format_integer_radix(bits, false, radix_u32),
        _ => {
            let val = f64::from_bits(bits);
            if val.is_nan() || val.is_infinite() || val == 0.0 {
                return std_number_to_string(tag, bits);
            }
            format_float_radix(val.abs(), val < 0.0, radix_u32)
        }
    }
}

/// Round |value| * 10^digits exactly, choosing the larger integer at a tie.
fn scaled_decimal(mantissa: u64, exponent: i32, digits: u32) -> BigUint {
    let mut scaled = BigUint::from_u64(mantissa);
    for _ in 0..digits {
        scaled.mul_add(5, 0);
    }
    let shift = exponent + digits as i32;
    if shift >= 0 {
        scaled.shl(shift as u32);
    } else {
        let mut bits = shift.unsigned_abs();
        while bits > 31 {
            scaled.div_rem_u32(1 << 31);
            bits -= 31;
        }
        let divisor = 1 << bits;
        let remainder = scaled.div_rem_u32(divisor);
        scaled.mul_add(1, u32::from(remainder >= divisor / 2));
    }
    scaled
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_to_fixed(
    tag: i32,
    bits: u64,
    digits: f64,
    range_message: *mut u8,
) -> *mut u8 {
    let digits = crate::text::integer(digits);
    if !(0..=100).contains(&digits) {
        crate::runtime::raise_string(crate::runtime::RANGE_ERROR, range_message);
        return core::ptr::null_mut();
    }
    let (negative, mantissa, exponent) = match tag {
        numeric::SIGNED => {
            let value = bits as i64;
            (value < 0, value.unsigned_abs(), 0)
        }
        numeric::UNSIGNED => (false, bits, 0),
        _ => {
            let value = f64::from_bits(bits);
            if !value.is_finite() || value.abs() >= 1e21 {
                return std_number_to_string(tag, bits);
            }
            let (mantissa, exponent) = binary_magnitude(value.abs());
            (value < 0.0, mantissa, exponent)
        }
    };
    let mut scaled = scaled_decimal(mantissa, exponent, digits as u32);
    // Sign, up to 21 integer digits, point and up to 100 fractional digits.
    let mut text = [0; 123];
    let mut at = text.len();
    for _ in 0..digits {
        at -= 1;
        text[at] = b'0' + scaled.div_rem_u32(10) as u8;
    }
    if digits > 0 {
        at -= 1;
        text[at] = b'.';
    }
    loop {
        at -= 1;
        text[at] = b'0' + scaled.div_rem_u32(10) as u8;
        if scaled.is_zero() {
            break;
        }
    }
    if negative {
        at -= 1;
        text[at] = b'-';
    }
    crate::runtime::from_bytes(&text[at..])
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_parse_int(
    string: *mut u8,
    radix_tag: i32,
    radix_bits: u64,
    output: *mut u64,
) -> i32 {
    // Compiled code only emits tags it defines; an unknown one is a compiler bug.
    let Ok(radix_tag) = numeric::Tag::try_from(radix_tag) else {
        panic!("invalid numeric tag")
    };
    let radix = Number::from_numeric(radix_tag, radix_bits);
    let (tag, bits) = parse_integer(view(string).text, radix).into_numeric();
    unsafe { *output = bits };
    tag
}
