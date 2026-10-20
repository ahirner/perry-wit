//! Numeric tokens and exact integer conversion shared by JSON and text-number built-ins.

use crate::layout::numeric::Tag;

mod big;
mod decimal;
#[cfg(test)]
mod tests;
pub use big::Big;
pub use decimal::{DecimalLiteral, DecimalSyntax, scan_decimal, split_sign};

/// Decimal facts used to recognize and convert exact integers.
#[derive(Default, Clone, Copy)]
pub struct Decimal {
    count: usize,
    trailing: usize,
    fraction_digits: i64,
}

impl Decimal {
    fn digit(&mut self, byte: u8, fractional: bool) {
        self.fraction_digits += i64::from(fractional);
        if self.count == 0 && byte == b'0' {
            return;
        }
        self.count += 1;
        self.trailing = if byte == b'0' { self.trailing + 1 } else { 0 };
    }

    pub fn finish_u64(&self, mantissa: &[u8], negative: bool, exponent: i64) -> Option<u64> {
        let significant = self.count - self.trailing;
        if significant == 0 {
            return Some(0);
        }
        if negative {
            return None;
        }
        let exponent10 = exponent - self.fraction_digits + self.trailing as i64;
        if exponent10 < 0 || significant as i64 + exponent10 > 20 {
            return None;
        }
        let mut value = 0u64;
        for digit in Digits::new(mantissa).iter().take(significant) {
            value = value.checked_mul(10)?.checked_add(u64::from(digit))?;
        }
        for _ in 0..exponent10 {
            value = value.checked_mul(10)?;
        }
        Some(value)
    }

    pub fn finish_i64(&self, mantissa: &[u8], negative: bool, exponent: i64) -> Option<i64> {
        let significant = self.count - self.trailing;
        if significant == 0 {
            return Some(0);
        }
        let mag = self.finish_u64(mantissa, false, exponent)?;
        if negative {
            i64::MIN.checked_add_unsigned((1u64 << 63).checked_sub(mag)?)
        } else {
            i64::try_from(mag).ok()
        }
    }
}

/// The exponent part (`e+12`), saturated far outside the double range.
fn parse_exponent(part: &[u8]) -> i64 {
    let Some((_, rest)) = part.split_first() else {
        return 0;
    };
    let (negative, rest) = split_sign(rest);
    let mut value = 0i64;
    for &digit in rest {
        value = (value * 10 + i64::from(digit - b'0')).min(1 << 40);
    }
    if negative { -value } else { value }
}

/// The significant digits of a mantissa (no sign, no exponent), skipping the point and
/// leading zeros.
struct Digits<'a> {
    text: &'a [u8],
}

impl<'a> Digits<'a> {
    fn new(mantissa: &'a [u8]) -> Self {
        let start = mantissa
            .iter()
            .position(|&b| b != b'0' && b != b'.')
            .unwrap_or(mantissa.len());
        Digits {
            text: &mantissa[start..],
        }
    }

    fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        self.text
            .iter()
            .filter(|&&b| b != b'.')
            .map(|&b| u32::from(b - b'0'))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Number {
    Float(f64),
    Signed(i64),
    Unsigned(u64),
}

impl Number {
    /// Explicit numeric conversion may round exact integer payloads.
    pub fn as_f64(self) -> f64 {
        match self {
            Self::Float(value) => value,
            Self::Signed(value) => value as f64,
            Self::Unsigned(value) => value as f64,
        }
    }

    fn to_int32(self) -> i32 {
        match self {
            Number::Signed(value) => value as i32,
            Number::Unsigned(value) => value as i32,
            Number::Float(value) => {
                if !value.is_finite() || value == 0.0 {
                    return 0;
                }
                let bits = value.to_bits();
                let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
                if !(0..84).contains(&exp) {
                    return 0;
                }
                let mantissa = (bits & ((1u64 << 52) - 1)) | (1u64 << 52);
                let low = if exp <= 52 {
                    (mantissa >> (52 - exp)) as i32
                } else {
                    (mantissa as u32).wrapping_shl((exp - 52) as u32) as i32
                };
                if (bits >> 63) != 0 {
                    low.wrapping_neg()
                } else {
                    low
                }
            }
        }
    }

    /// The number a tagged payload carries. Fixed-width payloads arrive sign- or zero-extended
    /// to 64 bits, so they share the exact 64-bit representations.
    pub fn from_numeric(tag: Tag, bits: u64) -> Self {
        match tag {
            Tag::Signed | Tag::S8 | Tag::S16 | Tag::S32 | Tag::S64 => Self::Signed(bits as i64),
            Tag::Unsigned | Tag::U8 | Tag::U16 | Tag::U32 | Tag::U64 => Self::Unsigned(bits),
            Tag::Null => Self::Float(0.0),
            Tag::Undefined | Tag::Omitted => Self::Float(f64::NAN),
            Tag::Float => Self::Float(f64::from_bits(bits)),
        }
    }

    pub fn into_numeric(self) -> (i32, u64) {
        match self {
            Self::Float(value) => (crate::layout::numeric::FLOAT, value.to_bits()),
            Self::Signed(value) => (crate::layout::numeric::SIGNED, value as u64),
            Self::Unsigned(value) => (crate::layout::numeric::UNSIGNED, value),
        }
    }
}

/// ECMAScript WhiteSpace and LineTerminator code points.
pub fn space(scalar: char) -> bool {
    let mut bytes = [0; 4];
    crate::whitespace::prefix(scalar.encode_utf8(&mut bytes).as_bytes()) != 0
}

pub fn ascii_digit(byte: u8, radix: u32) -> Option<u32> {
    let digit = match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'z' => u32::from(byte - b'a') + 10,
        b'A'..=b'Z' => u32::from(byte - b'A') + 10,
        _ => return None,
    };
    (digit < radix).then_some(digit)
}

/// Format any 64-bit magnitude; 66 bytes accommodate a sign and all binary digits.
pub fn format_integer(magnitude: u64, negative: bool, radix: u32, digits: &mut [u8; 66]) -> &[u8] {
    assert!((2..=36).contains(&radix), "invalid radix");
    let mut magnitude = magnitude;
    let mut at = digits.len();
    loop {
        at -= 1;
        let digit = (magnitude % u64::from(radix)) as u8;
        digits[at] = if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        };
        magnitude /= u64::from(radix);
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        at -= 1;
        digits[at] = b'-';
    }
    &digits[at..]
}

/// ECMAScript `parseInt(string, radix)` under the 64-bit exact integer contract.
pub fn parse_integer(input: &str, radix: Number) -> Number {
    let trimmed = input.trim_start_matches(space).as_bytes();
    let (negative, mut rest) = split_sign(trimmed);
    let r = radix.to_int32();
    let (radix, strip_prefix) = if r == 0 {
        (10u32, true)
    } else if !(2..=36).contains(&r) {
        return Number::Float(f64::NAN);
    } else {
        (r as u32, r == 16)
    };
    let radix =
        if strip_prefix && rest.len() >= 2 && rest[0] == b'0' && matches!(rest[1], b'x' | b'X') {
            rest = &rest[2..];
            16
        } else {
            radix
        };
    let limit = if negative { 1u64 << 63 } else { u64::MAX };
    let mut count = 0usize;
    let mut magnitude = 0u64;
    let mut overflow = false;
    for &byte in rest {
        let Some(digit) = ascii_digit(byte, radix) else {
            break;
        };
        count += 1;
        if !overflow {
            if let Some(next) = magnitude
                .checked_mul(u64::from(radix))
                .and_then(|v| v.checked_add(u64::from(digit)))
                .filter(|&v| v <= limit)
            {
                magnitude = next;
            } else {
                overflow = true;
            }
        }
    }
    if count == 0 || overflow {
        Number::Float(f64::NAN)
    } else if negative {
        if magnitude == 0 {
            Number::Float(-0.0)
        } else {
            Number::Signed(magnitude.wrapping_neg() as i64)
        }
    } else if magnitude <= i64::MAX as u64 {
        Number::Signed(magnitude as i64)
    } else {
        Number::Unsigned(magnitude)
    }
}
