//! Dynamic numeric helpers (`parseInt`, `Number.prototype.toString`).

use crate::text::view;
use perry_abi::layout::numeric;
use perry_abi::number::{Number, parse_integer};

fn format_integer(mut magnitude: u64, negative: bool) -> *mut u8 {
    let mut digits = [0; 21];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        at -= 1;
        digits[at] = b'-';
    }
    crate::runtime::from_bytes(&digits[at..])
}

#[inline(always)]
fn digit_byte(d: u8) -> u8 {
    if d < 10 { b'0' + d } else { b'a' + (d - 10) }
}

fn format_integer_radix(mut magnitude: u64, negative: bool, radix: u32) -> *mut u8 {
    let mut digits = [0u8; 66];
    let mut at = digits.len();
    let r = u64::from(radix);
    loop {
        at = at.wrapping_sub(1);
        if let Some(slot) = digits.get_mut(at) {
            *slot = digit_byte((magnitude % r) as u8);
        }
        magnitude /= r;
        if magnitude == 0 || at == 0 {
            break;
        }
    }
    if negative && at > 0 {
        at -= 1;
        if let Some(slot) = digits.get_mut(at) {
            *slot = b'-';
        }
    }
    crate::runtime::from_bytes(digits.get(at..).unwrap_or(&[]))
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_number_to_string(tag: i32, bits: u64) -> *mut u8 {
    match tag {
        numeric::SIGNED => {
            let value = bits as i64;
            format_integer(value.unsigned_abs(), value < 0)
        }
        numeric::UNSIGNED => format_integer(bits, false),
        _ => {
            let mut buf = [0u8; 40];
            let len = crate::runtime::number_format(f64::from_bits(bits), &mut buf);
            crate::runtime::from_bytes(buf.get(..len).unwrap_or(&[]))
        }
    }
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
            let neg = val < 0.0;
            let abs = val.abs();
            if libm::trunc(abs) == abs && abs <= (u64::MAX as f64) {
                return format_integer_radix(abs as u64, neg, radix_u32);
            }
            let base = f64::from(radix_u32);
            let mut int_part = libm::trunc(abs);
            let mut frac_part = abs - int_part;
            let mut out = crate::runtime::Buffer::new();
            if neg {
                out.push(b'-');
            }
            if int_part == 0.0 {
                out.push(b'0');
            } else {
                let mut rev_digits = [0u8; 1025];
                let mut at = rev_digits.len();
                while int_part >= 1.0 && at > 0 {
                    at -= 1;
                    let rem = ((int_part % base) as u8).min(35);
                    if let Some(slot) = rev_digits.get_mut(at) {
                        *slot = digit_byte(rem);
                    }
                    int_part = libm::trunc(int_part / base);
                }
                out.extend_from_slice(rev_digits.get(at..).unwrap_or(&[]));
            }
            if frac_part > 0.0 {
                out.push(b'.');
                for _ in 0..52 {
                    frac_part *= base;
                    let digit_f = libm::trunc(frac_part);
                    let digit = (digit_f as u8).min(35);
                    out.push(digit_byte(digit));
                    frac_part -= digit_f;
                    if frac_part == 0.0 {
                        break;
                    }
                }
            }
            crate::runtime::from_bytes(out.as_slice())
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
    let radix = Number::from_numeric(radix_tag, radix_bits);
    let (tag, bits) = parse_integer(view(string).text, radix).into_numeric();
    unsafe { *output = bits };
    tag
}

#[cfg(test)]
mod tests {
    use super::{Number, parse_integer};

    #[test]
    fn parse_integer_handles_exact_64_bit_bounds_and_overflow() {
        assert_eq!(
            parse_integer("9007199254740993", Number::Float(f64::NAN)),
            Number::Signed(9_007_199_254_740_993)
        );
        assert_eq!(
            parse_integer("9223372036854775807", Number::Float(f64::NAN)),
            Number::Signed(i64::MAX)
        );
        assert_eq!(
            parse_integer("9223372036854775808", Number::Float(f64::NAN)),
            Number::Unsigned(1 << 63)
        );
        assert_eq!(
            parse_integer("18446744073709551615", Number::Float(f64::NAN)),
            Number::Unsigned(u64::MAX)
        );
        assert_eq!(
            parse_integer("-9223372036854775808", Number::Float(f64::NAN)),
            Number::Signed(i64::MIN)
        );
        assert!(matches!(
            parse_integer("18446744073709551616", Number::Float(f64::NAN)),
            Number::Float(v) if v.is_nan()
        ));
        assert!(matches!(
            parse_integer("-9223372036854775809", Number::Float(f64::NAN)),
            Number::Float(v) if v.is_nan()
        ));
    }

    #[test]
    fn parse_integer_wraps_radix_by_to_int32() {
        assert_eq!(
            parse_integer("10", Number::Float(4_294_967_298.0)),
            Number::Signed(2)
        );
        assert_eq!(
            parse_integer("10", Number::Unsigned(4_294_967_298)),
            Number::Signed(2)
        );
        assert_eq!(
            parse_integer("0x10", Number::Float(4_294_967_296.0)),
            Number::Signed(16)
        );
        assert!(matches!(
            parse_integer("10", Number::Float(37.0)),
            Number::Float(v) if v.is_nan()
        ));
    }
}
