//! Dynamic numeric helpers (`parseInt`, `Number.prototype.toString`).

use crate::text::view;
use perry_abi::layout::numeric;
use perry_abi::number::{Number, parse_integer};

#[inline(always)]
fn digit_byte(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn format_integer_radix(magnitude: u64, negative: bool, radix: u32) -> *mut u8 {
    let mut digits = [0; 66];
    crate::runtime::from_bytes(perry_abi::number::format_integer(
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

type BigUint = perry_abi::number::Big<40>;

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_inspect(tag: i32, bits: u64) -> *mut u8 {
    if tag == numeric::FLOAT && bits == (-0.0f64).to_bits() {
        crate::runtime::from_static("-0")
    } else {
        std_number_to_string(tag, bits)
    }
}

fn format_float_radix(abs: f64, neg: bool, radix: u32) -> *mut u8 {
    // Exact integer fast path strictly below 2^64 (18446744073709551616.0 == 2^64 as f64).
    if libm::trunc(abs) == abs && abs < 18446744073709551616.0 {
        return format_integer_radix(abs as u64, neg, radix);
    }
    let bits = abs.to_bits();
    let biased_exp = ((bits >> 52) & 0x7ff) as i32;
    let frac_bits = bits & ((1u64 << 52) - 1);
    let (mantissa, exp2) = if biased_exp == 0 {
        (frac_bits, -1074)
    } else {
        (frac_bits | (1u64 << 52), biased_exp - 1075)
    };
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
        crate::runtime::raise(crate::runtime::RANGE_ERROR, "Invalid radix");
        return crate::runtime::from_static("");
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
