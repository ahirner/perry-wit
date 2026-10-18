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

struct BigUint {
    limbs: [u32; 40],
    len: usize,
}

impl BigUint {
    const fn zero() -> Self {
        Self {
            limbs: [0; 40],
            len: 0,
        }
    }

    fn from_u64(v: u64) -> Self {
        if v == 0 {
            return Self::zero();
        }
        let lo = v as u32;
        let hi = (v >> 32) as u32;
        let mut out = Self::zero();
        out.limbs[0] = lo;
        if hi != 0 {
            out.limbs[1] = hi;
            out.len = 2;
        } else {
            out.len = 1;
        }
        out
    }

    fn is_zero(&self) -> bool {
        self.len == 0
    }

    fn normalize(&mut self) {
        while self.len > 0 && self.limbs[self.len - 1] == 0 {
            self.len -= 1;
        }
    }

    fn shl_bits(&mut self, bits: usize) {
        if self.is_zero() || bits == 0 {
            return;
        }
        let word_shift = bits / 32;
        let bit_shift = bits % 32;
        let old_len = self.len;
        if bit_shift == 0 {
            let mut i = old_len;
            while i > 0 {
                i -= 1;
                self.limbs[i + word_shift] = self.limbs[i];
            }
            let mut j = 0;
            while j < word_shift {
                self.limbs[j] = 0;
                j += 1;
            }
            self.len = old_len + word_shift;
        } else {
            let carry_top = self.limbs[old_len - 1] >> (32 - bit_shift);
            let mut i = old_len - 1;
            while i > 0 {
                self.limbs[i + word_shift] =
                    (self.limbs[i] << bit_shift) | (self.limbs[i - 1] >> (32 - bit_shift));
                i -= 1;
            }
            self.limbs[word_shift] = self.limbs[0] << bit_shift;
            let mut j = 0;
            while j < word_shift {
                self.limbs[j] = 0;
                j += 1;
            }
            self.len = old_len + word_shift;
            if carry_top != 0 {
                self.limbs[self.len] = carry_top;
                self.len += 1;
            }
        }
        self.normalize();
    }

    fn mul_u32(&mut self, factor: u32) {
        if self.is_zero() || factor == 1 {
            return;
        }
        if factor == 0 {
            self.len = 0;
            return;
        }
        let mut carry = 0u64;
        let mut i = 0;
        while i < self.len {
            let prod = u64::from(self.limbs[i]) * u64::from(factor) + carry;
            self.limbs[i] = prod as u32;
            carry = prod >> 32;
            i += 1;
        }
        if carry != 0 {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
    }

    fn div_rem_u32(&mut self, divisor: u32) -> u32 {
        let mut rem = 0u64;
        let mut i = self.len;
        while i > 0 {
            i -= 1;
            let cur = (rem << 32) | u64::from(self.limbs[i]);
            self.limbs[i] = (cur / u64::from(divisor)) as u32;
            rem = cur % u64::from(divisor);
        }
        self.normalize();
        rem as u32
    }

    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        if self.len != other.len {
            return self.len.cmp(&other.len);
        }
        let mut i = self.len;
        while i > 0 {
            i -= 1;
            if self.limbs[i] != other.limbs[i] {
                return self.limbs[i].cmp(&other.limbs[i]);
            }
        }
        core::cmp::Ordering::Equal
    }

    fn add_assign(&mut self, other: &Self) {
        let max_len = if self.len > other.len {
            self.len
        } else {
            other.len
        };
        let mut carry = 0u64;
        let mut i = 0;
        while i < max_len {
            let a = if i < self.len {
                u64::from(self.limbs[i])
            } else {
                0
            };
            let b = if i < other.len {
                u64::from(other.limbs[i])
            } else {
                0
            };
            let sum = a + b + carry;
            self.limbs[i] = sum as u32;
            carry = sum >> 32;
            i += 1;
        }
        self.len = max_len;
        if carry != 0 {
            self.limbs[self.len] = carry as u32;
            self.len += 1;
        }
        self.normalize();
    }

    fn sub_assign(&mut self, other: &Self) {
        let mut borrow = 0i64;
        let mut i = 0;
        while i < self.len {
            let a = i64::from(self.limbs[i]);
            let b = if i < other.len {
                i64::from(other.limbs[i])
            } else {
                0
            };
            let diff = a - b - borrow;
            if diff < 0 {
                self.limbs[i] = (diff + (1i64 << 32)) as u32;
                borrow = 1;
            } else {
                self.limbs[i] = diff as u32;
                borrow = 0;
            }
            i += 1;
        }
        self.normalize();
    }

    fn div_rem_small_quot(&mut self, divisor: &Self) -> u32 {
        let mut q = 0u32;
        while self.cmp(divisor) != core::cmp::Ordering::Less {
            self.sub_assign(divisor);
            q += 1;
        }
        q
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
        b.shl_bits(exp2 as usize);
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
        while !int_val.is_zero() && at > 0 {
            at -= 1;
            let rem = int_val.div_rem_u32(radix) as u8;
            if let Some(slot) = rev_digits.get_mut(at) {
                *slot = digit_byte(rem);
            }
        }
        out.extend_from_slice(rev_digits.get(at..).unwrap_or(&[]));
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
            r.shl_bits(1);
            let mut s = BigUint::from_u64(1);
            s.shl_bits(shift + 1);
            let mut delta = BigUint::from_u64(1);

            loop {
                r.mul_u32(radix);
                delta.mul_u32(radix);
                let mut digit = r.div_rem_small_quot(&s) as u8;

                let low = match r.cmp(&delta) {
                    core::cmp::Ordering::Less => true,
                    core::cmp::Ordering::Equal => even,
                    core::cmp::Ordering::Greater => false,
                };
                let mut sum = BigUint {
                    limbs: r.limbs,
                    len: r.len,
                };
                sum.add_assign(&delta);
                let high = match sum.cmp(&s) {
                    core::cmp::Ordering::Greater => true,
                    core::cmp::Ordering::Equal => even,
                    core::cmp::Ordering::Less => false,
                };

                if low || high || r.is_zero() {
                    if (high && !low)
                        || (high && low && {
                            let mut two_r = BigUint {
                                limbs: r.limbs,
                                len: r.len,
                            };
                            two_r.shl_bits(1);
                            two_r.cmp(&s) == core::cmp::Ordering::Greater
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
