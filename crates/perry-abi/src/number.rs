//! Correctly rounded decimal-to-double conversion shared by JSON number parsing and text-to-number
//! built-ins (`parseFloat`, `Number(string)`).
//!
//! A literal is `D × 10^e` with `D` its significant digits. When `D < 2^53` and `10^|e|` is
//! exact (Clinger's fast path, nearly every literal in practice) one multiplication or
//! division rounds correctly. Otherwise an approximation from the leading 19 digits is
//! corrected by exact comparison against the midpoints of neighbouring doubles, on a stack
//! big integer. Digits after the 780th only decide ties: a midpoint has at most 767
//! significant digits, so they act as one sticky digit.

use crate::layout::numeric::Tag;

/// Maximum significant digits kept exactly; further digits only mark the value as above.
const KEPT_DIGITS: usize = 780;
/// 32-bit limbs: `10^780 · 2^1100 · 5^310` and `2^1100 · 5^1104 · 2^54` both fit.
const LIMBS: usize = 180;

const EXACT: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Decimal facts accumulated by a single-pass lexer; no second mantissa scan is required on the
/// fast path.
#[derive(Default, Clone, Copy)]
pub struct Decimal {
    count: usize,
    trailing: usize,
    leading: u64,
    fraction_digits: i64,
}

impl Decimal {
    pub fn digit(&mut self, byte: u8, fractional: bool) {
        self.fraction_digits += i64::from(fractional);
        if self.count == 0 && byte == b'0' {
            return;
        }
        self.count += 1;
        if self.count <= 19 {
            self.leading = self.leading * 10 + u64::from(byte - b'0');
        }
        self.trailing = if byte == b'0' { self.trailing + 1 } else { 0 };
    }

    pub fn finish(mut self, mantissa: &[u8], negative: bool, exponent: i64) -> f64 {
        let significant = self.count - self.trailing;
        let exponent10 = exponent - self.fraction_digits + self.trailing as i64;
        if significant < self.count.min(19) {
            self.leading /= 10u64.pow((self.count.min(19) - significant) as u32);
        }
        let magnitude = if significant == 0 {
            0.0
        } else {
            convert(mantissa, significant, exponent10, self.leading)
        };
        if negative { -magnitude } else { magnitude }
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
            if mag == (i64::MAX as u64) + 1 {
                Some(i64::MIN)
            } else if mag <= i64::MAX as u64 {
                Some(-(mag as i64))
            } else {
                None
            }
        } else if mag <= i64::MAX as u64 {
            Some(mag as i64)
        } else {
            None
        }
    }
}

/// The exponent part (`e+12`), saturated far outside the double range.
pub fn parse_exponent(part: &[u8]) -> i64 {
    let Some((_, rest)) = part.split_first() else {
        return 0;
    };
    let (negative, rest) = match rest.first() {
        Some(b'-') => (true, &rest[1..]),
        Some(b'+') => (false, &rest[1..]),
        _ => (false, rest),
    };
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

/// `D × 10^e` for the first `significant` digits (`D` without leading or trailing zeros).
fn convert(mantissa: &[u8], significant: usize, exponent10: i64, leading: u64) -> f64 {
    // Out of range either way: below half the smallest subnormal, or above the largest double.
    let scale = significant as i64 + exponent10;
    if scale <= -324 {
        return 0.0;
    }
    if scale > 310 {
        return f64::INFINITY;
    }
    let exponent10 = exponent10 as i32;
    if significant <= 19 && leading < 1 << 53 {
        if (0..=22).contains(&exponent10) {
            return leading as f64 * EXACT[exponent10 as usize];
        }
        if (-22..0).contains(&exponent10) {
            return leading as f64 / EXACT[(-exponent10) as usize];
        }
        // `1e30`: move powers of ten into the integer while it stays exact.
        let mut integer = leading;
        let mut rest = exponent10;
        while rest > 22 && integer < (1 << 53) / 10 {
            integer *= 10;
            rest -= 1;
        }
        if (0..=22).contains(&rest) {
            return integer as f64 * EXACT[rest as usize];
        }
    }
    let approximate = scale10(
        leading as f64,
        exponent10 + significant.saturating_sub(19) as i32,
    );
    correct(&Digits::new(mantissa), significant, exponent10, approximate)
}

/// `x × 10^power` in a few roundings, without overflowing or underflowing on the way.
fn scale10(mut x: f64, mut power: i32) -> f64 {
    while power != 0 {
        let step = power.clamp(-300, 300);
        let factor = pow10(step.unsigned_abs());
        x = if step > 0 { x * factor } else { x / factor };
        power -= step;
    }
    x
}

fn pow10(mut power: u32) -> f64 {
    let (mut result, mut base) = (1.0, 10.0f64);
    while power > 0 {
        if power & 1 != 0 {
            result *= base;
        }
        base *= base;
        power >>= 1;
    }
    result
}

/// A positive double (or the step past the largest) as `mantissa × 2^exponent`.
fn decompose(bits: u64) -> (u64, i32) {
    let biased = (bits >> 52) as i32;
    let fraction = bits & ((1 << 52) - 1);
    if biased == 0 {
        (fraction, -1074)
    } else {
        (fraction | 1 << 52, biased - 1075)
    }
}

/// Exact comparison of `D × 10^e` (digits beyond [KEPT_DIGITS] only make it larger) with the
/// midpoint of the adjacent doubles `low` and `low + 1` (by bits), then step until the
/// candidate is the nearest.
#[inline(never)]
fn correct(digits: &Digits<'_>, significant: usize, exponent10: i32, approximate: f64) -> f64 {
    let kept = significant.min(KEPT_DIGITS);
    let sticky = digits.iter().skip(kept).any(|digit| digit != 0);
    let exponent10 = exponent10 + (significant - kept) as i32;
    let mut value = Big::zero();
    let mut chunk = 0u32;
    let mut chunk_scale = 1u32;
    for digit in digits.iter().take(kept) {
        chunk = chunk * 10 + digit;
        chunk_scale *= 10;
        if chunk_scale == 1_000_000_000 {
            value.mul_add(chunk_scale, chunk);
            (chunk, chunk_scale) = (0, 1);
        }
    }
    value.mul_add(chunk_scale, chunk);
    // A positive `5^e` scales D once; a negative one scales each midpoint.
    if exponent10 >= 0 {
        value.mul_pow5(exponent10 as u32);
    }
    // `D × 10^e` against `low_mid × 2^k`: compare D × 5^e × 2^e and the midpoint scaled.
    let compare = |low: u64| -> core::cmp::Ordering {
        let (m0, k0) = decompose(low);
        let (m1, k1) = decompose(low + 1);
        let k = k0.min(k1);
        let midpoint = (m0 << (k0 - k)) + (m1 << (k1 - k));
        let exponent2 = k - 1;
        let mut left = value.clone();
        let mut right = Big::from_u64(midpoint);
        let (mut left_shift, mut right_shift) = if exponent10 >= 0 {
            (exponent10, 0)
        } else {
            right.mul_pow5(exponent10.unsigned_abs());
            (0, -exponent10)
        };
        if exponent2 >= 0 {
            right_shift += exponent2;
        } else {
            left_shift -= exponent2;
        }
        let common = left_shift.min(right_shift);
        left.shl((left_shift - common) as u32);
        right.shl((right_shift - common) as u32);
        match left.cmp(&right) {
            core::cmp::Ordering::Equal if sticky => core::cmp::Ordering::Greater,
            order => order,
        }
    };
    const INFINITY: u64 = 0x7ff0_0000_0000_0000;
    let mut bits = approximate.to_bits().min(INFINITY - 1);
    // Up while the value is past the midpoint above (ties go to the even mantissa).
    loop {
        if bits == INFINITY {
            break;
        }
        match compare(bits) {
            core::cmp::Ordering::Greater => bits += 1,
            core::cmp::Ordering::Equal if bits & 1 == 1 => bits += 1,
            _ => break,
        }
    }
    // Down while the value is below the midpoint beneath.
    while bits > 0 {
        match compare(bits - 1) {
            core::cmp::Ordering::Less => bits -= 1,
            core::cmp::Ordering::Equal if bits & 1 == 1 => bits -= 1,
            _ => break,
        }
    }
    f64::from_bits(bits)
}

/// A fixed-capacity unsigned big integer (little-endian 32-bit limbs).
#[derive(Clone)]
struct Big {
    limbs: [u32; LIMBS],
    len: usize,
}

impl Big {
    fn zero() -> Self {
        Big {
            limbs: [0; LIMBS],
            len: 0,
        }
    }

    fn from_u64(value: u64) -> Self {
        let mut big = Big::zero();
        if value != 0 {
            big.limbs[0] = value as u32;
            big.len = 1;
            if value >> 32 != 0 {
                big.limbs[1] = (value >> 32) as u32;
                big.len = 2;
            }
        }
        big
    }

    /// `self × factor + addend`.
    fn mul_add(&mut self, factor: u32, addend: u32) {
        let mut carry = u64::from(addend);
        for limb in &mut self.limbs[..self.len] {
            let product = u64::from(*limb) * u64::from(factor) + carry;
            *limb = product as u32;
            carry = product >> 32;
        }
        if carry != 0 && self.len < LIMBS {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
    }

    fn mul_pow5(&mut self, mut power: u32) {
        const POW5_13: u32 = 1_220_703_125;
        while power >= 13 {
            self.mul_add(POW5_13, 0);
            power -= 13;
        }
        self.mul_add(5u32.pow(power), 0);
    }

    fn shl(&mut self, bits: u32) {
        if self.len == 0 {
            return;
        }
        let (words, bits) = ((bits / 32) as usize, bits % 32);
        let previous_len = self.len;
        let len = (previous_len + words + 1).min(LIMBS);
        for limb in &mut self.limbs[previous_len..len] {
            *limb = 0;
        }
        self.len = len;
        for index in (0..len).rev() {
            let source = index as isize - words as isize;
            let high = if source >= 0 && (source as usize) < previous_len {
                self.limbs[source as usize]
            } else {
                0
            };
            let low = if source >= 1 && (source as usize - 1) < previous_len {
                self.limbs[source as usize - 1]
            } else {
                0
            };
            self.limbs[index] = if bits == 0 {
                high
            } else {
                (high << bits) | (low >> (32 - bits))
            };
        }
        while self.len > 0 && self.limbs[self.len - 1] == 0 {
            self.len -= 1;
        }
    }

    fn cmp(&self, other: &Big) -> core::cmp::Ordering {
        self.len.cmp(&other.len).then_with(|| {
            self.limbs[..self.len]
                .iter()
                .rev()
                .cmp(other.limbs[..other.len].iter().rev())
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Number {
    Float(f64),
    Signed(i64),
    Unsigned(u64),
}

impl Number {
    pub fn to_int32(self) -> i32 {
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

impl PartialEq for Number {
    fn eq(&self, other: &Self) -> bool {
        match (*self, *other) {
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Signed(left), Self::Signed(right)) => left == right,
            (Self::Unsigned(left), Self::Unsigned(right)) => left == right,
            _ => false,
        }
    }
}

/// ECMAScript WhiteSpace and LineTerminator code points.
pub fn space(scalar: char) -> bool {
    matches!(
        scalar,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

fn ascii_digit(byte: u8, radix: u32) -> Option<u32> {
    let digit = match byte {
        b'0'..=b'9' => u32::from(byte - b'0'),
        b'a'..=b'z' => u32::from(byte - b'a') + 10,
        b'A'..=b'Z' => u32::from(byte - b'A') + 10,
        _ => return None,
    };
    (digit < radix).then_some(digit)
}

/// ECMAScript `parseInt(string, radix)` under the 64-bit exact integer contract.
pub fn parse_integer(input: &str, radix: Number) -> Number {
    let trimmed = input.trim_start_matches(space).as_bytes();
    let (negative, mut rest) = match trimmed.first() {
        Some(b'-') => (true, &trimmed[1..]),
        Some(b'+') => (false, &trimmed[1..]),
        _ => (false, trimmed),
    };
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
