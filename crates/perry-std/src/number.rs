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
            crate::runtime::from_bytes(&buf[..len])
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
