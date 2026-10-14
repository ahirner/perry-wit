//! Correctly rounded decimal-to-double conversion for JSON number literals, small enough for
//! the helper: `core`'s `str::parse::<f64>` costs about 18 KB of code and tables.
//!
//! A literal is `D × 10^e` with `D` its significant digits. When `D < 2^53` and `10^|e|` is
//! exact (Clinger's fast path, nearly every literal in practice) one multiplication or
//! division rounds correctly. Otherwise an approximation from the leading 19 digits is
//! corrected by exact comparison against the midpoints of neighbouring doubles, on a stack
//! big integer. Digits after the 780th only decide ties: a midpoint has at most 767
//! significant digits, so they act as one sticky digit.
use arrayvec::ArrayVec;

/// Maximum significant digits kept exactly; further digits only mark the value as above.
const KEPT_DIGITS: usize = 780;
/// 32-bit limbs: `10^780 · 2^1100 · 5^310` and `2^1100 · 5^1104 · 2^54` both fit.
const LIMBS: usize = 180;

const EXACT: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Decimal facts accumulated by the JSON lexer; no second mantissa scan is required.
#[derive(Default)]
pub(super) struct Decimal {
    count: usize,
    trailing: usize,
    leading: u64,
    fraction_digits: i64,
}
impl Decimal {
    pub(super) fn digit(&mut self, byte: u8, fractional: bool) {
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
    pub(super) fn finish(mut self, mantissa: &[u8], negative: bool, exponent: i64) -> f64 {
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
}

#[cfg(test)]
fn parse_number(literal: &[u8]) -> f64 {
    let negative = literal.first() == Some(&b'-');
    let body = &literal[usize::from(negative)..];
    let mut at = 0;
    let mut fractional = false;
    let mut decimal = Decimal::default();
    while at < body.len() && !matches!(body[at], b'e' | b'E') {
        let byte = body[at];
        at += 1;
        if byte == b'.' {
            fractional = true;
        } else {
            decimal.digit(byte, fractional);
        }
    }
    decimal.finish(&body[..at], negative, parse_exponent(&body[at..]))
}

/// The exponent part (`e+12`), saturated far outside the double range.
#[cfg(test)]
fn parse_exponent(part: &[u8]) -> i64 {
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
    limbs: ArrayVec<u32, LIMBS>,
}

impl Big {
    fn zero() -> Self {
        Big {
            limbs: ArrayVec::new(),
        }
    }

    fn from_u64(value: u64) -> Self {
        let mut big = Big::zero();
        if value != 0 {
            big.limbs.push(value as u32);
            if value >> 32 != 0 {
                big.limbs.push((value >> 32) as u32);
            }
        }
        big
    }

    /// `self × factor + addend`.
    fn mul_add(&mut self, factor: u32, addend: u32) {
        let mut carry = u64::from(addend);
        for limb in &mut self.limbs {
            let product = u64::from(*limb) * u64::from(factor) + carry;
            *limb = product as u32;
            carry = product >> 32;
        }
        if carry != 0 && self.limbs.len() < LIMBS {
            self.limbs.push(carry as u32);
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
        if self.limbs.is_empty() {
            return;
        }
        let (words, bits) = ((bits / 32) as usize, bits % 32);
        let previous_len = self.limbs.len();
        let len = (previous_len + words + 1).min(LIMBS);
        while self.limbs.len() < len {
            self.limbs.push(0);
        }
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
        while self.limbs.last() == Some(&0) {
            self.limbs.pop();
        }
    }

    fn cmp(&self, other: &Big) -> core::cmp::Ordering {
        self.limbs
            .len()
            .cmp(&other.limbs.len())
            .then_with(|| self.limbs.iter().rev().cmp(other.limbs.iter().rev()))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_number;

    fn check(literal: &str) {
        let expected: f64 = literal.parse().unwrap();
        let actual = parse_number(literal.as_bytes());
        let mut reader = super::super::reader::Reader::new(literal);
        let unsigned = literal.trim_start_matches('-').as_bytes();
        let valid_json =
            unsigned[0] != b'0' || matches!(unsigned.get(1), None | Some(b'.' | b'e' | b'E'));
        let fused = reader.numeric().expect("generated decimal grammar");
        if valid_json {
            assert_eq!(reader.position, literal.len(), "fused token {literal}");
            assert_eq!(fused.to_bits(), expected.to_bits(), "fused {literal}");
        } else {
            assert!(
                reader.position < literal.len(),
                "JSON rejects leading zeros: {literal}"
            );
        }
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "{literal}: {actual:e} != {expected:e}"
        );
    }

    #[test]
    fn hard_cases_round_like_core() {
        for literal in [
            "0",
            "-0",
            "0.0",
            "1",
            "-1",
            "0.1",
            "0.3",
            "1e23",
            "8.98846567431158e307",
            "1.7976931348623157e308",
            "1.7976931348623158e308",
            "1.7976931348623159e308",
            "2e308",
            "4.9e-324",
            "2.4703282292062327e-324",
            "2.4703282292062328e-324",
            "2.2250738585072011e-308",
            "2.2250738585072012e-308",
            "5e-324",
            "1e-400",
            "9007199254740993",
            "9007199254740992.5",
            "123456789012345678901234567890",
            "0.000000000000000000000000000000000000001",
            "1e1000000000000",
            "1e-1000000000000",
            "7.038531e-26",
            "3.4028235677973366e38",
            "1.00000000000000011102230246251565404236316680908203125",
            "1.00000000000000011102230246251565404236316680908203124",
            "1.00000000000000011102230246251565404236316680908203126",
            "100000000000000000000000000000000000000000000000000000000000000000000001e-70",
            "123.456e7",
            "0.00001234e10",
            "1E+2",
            "1e-2",
            "100e-2",
            "1.000e2",
        ] {
            check(literal);
        }
        let long = format!("1.{}1e0", "0".repeat(900));
        check(&long);
        let tie = format!("9007199254740993{}", "0".repeat(800));
        check(&format!("{tie}e-800"));
        check(&format!("{tie}1e-801"));
    }

    #[test]
    fn random_literals_round_like_core() {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..200_000 {
            let r = next();
            let literal = match r % 4 {
                // Shortest round-trip forms of random doubles.
                0 => format!("{:e}", f64::from_bits(next() & 0x7fef_ffff_ffff_ffff)),
                // Random digit strings with random exponents.
                1 => {
                    let digits = 1 + (next() % 40) as usize;
                    let text: String = (0..digits)
                        .map(|_| char::from(b'0' + (next() % 10) as u8))
                        .collect();
                    format!("{text}e{}", (next() % 700) as i64 - 350)
                }
                // Long expansions of random doubles, cut at a random length.
                2 => {
                    let x = f64::from_bits(next() & 0x7fef_ffff_ffff_ffff);
                    format!("{:.*e}", 15 + (next() % 40) as usize, x)
                }
                _ => format!("{}", (next() % 1_000_000_000) as f64 / 1000.0),
            };
            check(&literal);
        }
    }
}
