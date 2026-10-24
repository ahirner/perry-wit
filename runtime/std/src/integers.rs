//! Immutable big integers with an inline 64-bit magnitude and optional higher limbs.
use crate::runtime;
use core::cmp::Ordering;

/// The first 16 bytes also represent WIT transport values and compiler-emitted constants.
/// Higher base-2^32 limbs follow the header in little-endian order, without pointers.
#[repr(C)]
pub(crate) struct Integer {
    magnitude: u64,
    negative: u32,
    extra_len: u32,
}

impl Integer {
    pub(crate) fn is_zero(&self) -> bool {
        self.magnitude == 0 && self.extra_len == 0
    }

    fn allocate(extra_len: u32, negative: bool) -> Option<&'static mut Self> {
        let Some(size) = extra_len
            .checked_mul(4)
            .and_then(|size| size.checked_add(16))
        else {
            runtime::raise(
                runtime::RANGE_ERROR,
                perry_rt_sys::static_text!("BigInt exceeds addressable memory"),
            );
            return None;
        };
        let result = runtime::alloc(size).cast::<Self>();
        // SAFETY: fresh aligned storage, exclusively owned until returned to compiled code.
        unsafe {
            result.write(Self {
                magnitude: 0,
                negative: u32::from(negative),
                extra_len,
            });
            core::ptr::write_bytes(result.add(1).cast::<u32>(), 0, extra_len as usize);
            Some(&mut *result)
        }
    }

    fn limbs(&self) -> impl DoubleEndedIterator<Item = u32> + '_ {
        // SAFETY: every runtime integer has extra_len initialized limbs after its header.
        let high = unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(self).add(1).cast::<u32>(),
                self.extra_len as usize,
            )
        };
        [self.magnitude as u32, (self.magnitude >> 32) as u32]
            .into_iter()
            .chain(high.iter().copied())
    }

    fn set_limb(&mut self, index: usize, limb: u32) {
        match index {
            0 => self.magnitude = (self.magnitude & !u64::from(u32::MAX)) | u64::from(limb),
            1 => self.magnitude = (self.magnitude & u64::from(u32::MAX)) | (u64::from(limb) << 32),
            _ => {
                // SAFETY: callers iterate within the allocated limb count.
                unsafe {
                    core::ptr::from_mut(self)
                        .add(1)
                        .cast::<u32>()
                        .add(index - 2)
                        .write(limb)
                };
            }
        }
    }

    fn normalize(&mut self) {
        self.extra_len -= self
            .limbs()
            .rev()
            .take(self.extra_len as usize)
            .take_while(|&limb| limb == 0)
            .count() as u32;
        if self.is_zero() {
            self.negative = 0;
        }
    }

    fn compare_magnitude(&self, other: &Self) -> Ordering {
        self.extra_len
            .cmp(&other.extra_len)
            .then_with(|| self.limbs().rev().cmp(other.limbs().rev()))
    }
}

/// Convert an integral finite number within the signed-magnitude transport domain.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bigint_from_number(value: f64) -> *mut Integer {
    let magnitude = libm::fabs(value);
    if !value.is_finite() || libm::trunc(value) != value || magnitude >= 18446744073709551616.0 {
        runtime::raise(
            runtime::RANGE_ERROR,
            perry_rt_sys::static_text!(
                "BigInt conversion requires an integer within the 64-bit transport domain"
            ),
        );
        return core::ptr::null_mut();
    }
    let Some(result) = Integer::allocate(0, value < 0.0) else {
        return core::ptr::null_mut();
    };
    result.magnitude = magnitude as u64;
    result
}

/// # Safety
/// `value` is a live integer owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_negate(value: *const Integer) -> *mut Integer {
    let value = unsafe { &*value };
    let Some(result) = Integer::allocate(value.extra_len, value.negative == 0) else {
        return core::ptr::null_mut();
    };
    for (index, limb) in value.limbs().enumerate() {
        result.set_limb(index, limb);
    }
    result.normalize();
    result
}

fn add(a: &Integer, b: &Integer, subtract: bool) -> *mut Integer {
    let right_negative = (b.negative != 0) ^ subtract;
    let same_sign = (a.negative != 0) == right_negative;
    let (large, small, negative) = if same_sign || a.compare_magnitude(b) != Ordering::Less {
        (a, b, a.negative != 0)
    } else {
        (b, a, right_negative)
    };
    let extra_len = a.extra_len.max(b.extra_len) + u32::from(same_sign);
    let Some(result) = Integer::allocate(extra_len, negative) else {
        return core::ptr::null_mut();
    };
    let mut carry = 0i64;
    let len = 2 + a.extra_len.max(b.extra_len) as usize;
    for (index, (left, right)) in large
        .limbs()
        .chain(core::iter::repeat(0))
        .zip(small.limbs().chain(core::iter::repeat(0)))
        .take(len)
        .enumerate()
    {
        let right = if same_sign {
            i64::from(right)
        } else {
            -i64::from(right)
        };
        let value = i64::from(left) + right + carry;
        result.set_limb(index, value as u32);
        carry = value >> 32;
    }
    if same_sign {
        result.set_limb(len, carry as u32);
    }
    result.normalize();
    result
}

/// # Safety
/// Both operands are live integers owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_add(
    a: *const Integer,
    b: *const Integer,
) -> *mut Integer {
    add(unsafe { &*a }, unsafe { &*b }, false)
}

/// # Safety
/// Both operands are live integers owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_sub(
    a: *const Integer,
    b: *const Integer,
) -> *mut Integer {
    add(unsafe { &*a }, unsafe { &*b }, true)
}

/// # Safety
/// Both operands are live integers owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_compare(a: *const Integer, b: *const Integer) -> i32 {
    let (a, b) = unsafe { (&*a, &*b) };
    let order = b.negative.cmp(&a.negative).then_with(|| {
        if a.negative != 0 {
            b.compare_magnitude(a)
        } else {
            a.compare_magnitude(b)
        }
    });
    match order {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// Explicit `Number(bigint)` conversion rounds once to nearest, ties to even.
///
/// # Safety
/// `value` is a live integer owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_to_number(value: *const Integer) -> f64 {
    let value = unsafe { &*value };
    let magnitude = if value.extra_len == 0 {
        value.magnitude as f64
    } else {
        let mut limbs = value.limbs().rev().skip_while(|&limb| limb == 0);
        let Some(first) = limbs.next() else {
            return 0.0;
        };
        let mut significant = u64::from(first);
        let mut bits = 32 - first.leading_zeros();
        let mut exponent = 0u32;
        let mut sticky = false;
        for limb in limbs {
            if bits < 54 {
                let take = (54 - bits).min(32);
                significant = (significant << take) | u64::from(limb >> (32 - take));
                bits += take;
                exponent = exponent.saturating_add(32 - take);
                sticky |= take < 32 && limb << take != 0;
            } else {
                exponent = exponent.saturating_add(32);
                sticky |= limb != 0;
            }
        }
        if bits == 54 {
            let round = significant & 1 != 0 && (sticky || significant & 2 != 0);
            significant = (significant >> 1) + u64::from(round);
            exponent = exponent.saturating_add(1);
        }
        libm::scalbn(significant as f64, exponent.min(i32::MAX as u32) as i32)
    };
    if value.negative == 0 {
        magnitude
    } else {
        -magnitude
    }
}

/// # Safety
/// `value` is a live integer owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_to_string(value: *const Integer) -> *mut u8 {
    let value = unsafe { &*value };
    if value.extra_len == 0 {
        let mut digits = [0; 66];
        return runtime::from_bytes(perry_rt_sys::number::format_integer(
            value.magnitude,
            value.negative != 0,
            10,
            &mut digits,
        ));
    }
    // Decimal division needs one mutable copy; the input remains immutable and reusable.
    let len = value.extra_len as usize + 2;
    let scratch = runtime::alloc((len * 4) as u32).cast::<u32>();
    let limbs = unsafe { core::slice::from_raw_parts_mut(scratch, len) };
    for (slot, limb) in limbs.iter_mut().zip(value.limbs()) {
        *slot = limb;
    }
    let mut len = len;
    let mut digits = runtime::Buffer::new();
    while len != 0 {
        let mut remainder = 0u64;
        for limb in limbs[..len].iter_mut().rev() {
            let dividend = (remainder << 32) | u64::from(*limb);
            *limb = (dividend / 10) as u32;
            remainder = dividend % 10;
        }
        digits.push(b'0' + remainder as u8);
        while len != 0 && limbs[len - 1] == 0 {
            len -= 1;
        }
    }
    if value.negative != 0 {
        digits.push(b'-');
    }
    let (end, _) = digits.spare();
    // SAFETY: the buffer exclusively owns len initialized ASCII bytes ending at end.
    let bytes = unsafe { core::slice::from_raw_parts_mut(end.sub(digits.len()), digits.len()) };
    bytes.reverse();
    unsafe { digits.into_string() }
}
