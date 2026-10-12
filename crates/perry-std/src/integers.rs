//! Immutable signed-magnitude values for exact WIT integer transport.
use crate::runtime;

/// Native and guest layout is identical; no pointer fields are stored here.
#[repr(C)]
pub(crate) struct Integer {
    pub(crate) magnitude: u64,
    pub(crate) negative: u32,
    padding: u32,
}

/// Convert an integral finite number within the signed-magnitude transport domain.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bigint_from_number(value: f64) -> *mut Integer {
    let magnitude = libm::fabs(value);
    if !value.is_finite() || libm::trunc(value) != value || magnitude >= 18446744073709551616.0 {
        runtime::raise(
            runtime::RANGE_ERROR,
            "BigInt conversion requires an integer within the 64-bit transport domain",
        );
        return core::ptr::null_mut();
    }
    let result = runtime::alloc(16).cast::<Integer>();
    // SAFETY: the runtime allocated a complete, aligned integer payload; no collection occurs here.
    unsafe {
        result.write(Integer {
            magnitude: magnitude as u64,
            negative: u32::from(value < 0.0),
            padding: 0,
        });
    }
    result
}

/// # Safety
/// `value` is a live integer owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_negate(value: *const Integer) -> *mut Integer {
    let value = unsafe { &*value };
    let result = runtime::alloc(16).cast::<Integer>();
    unsafe {
        result.write(Integer {
            magnitude: value.magnitude,
            negative: u32::from(value.magnitude != 0 && value.negative == 0),
            padding: 0,
        });
    }
    result
}

/// # Safety
/// Both operands are live integers owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_compare(a: *const Integer, b: *const Integer) -> i32 {
    let (a, b) = unsafe { (&*a, &*b) };
    let order = b.negative.cmp(&a.negative).then_with(|| {
        if a.negative != 0 {
            b.magnitude.cmp(&a.magnitude)
        } else {
            a.magnitude.cmp(&b.magnitude)
        }
    });
    match order {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    }
}

/// Explicit `Number(bigint)` conversion may round, as in ECMAScript.
///
/// # Safety
/// `value` is a live integer owned by compiled code.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_bigint_to_number(value: *const Integer) -> f64 {
    let value = unsafe { &*value };
    let magnitude = value.magnitude as f64;
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
    let mut digits = [0; 21];
    let mut at = digits.len();
    let mut magnitude = value.magnitude;
    loop {
        at -= 1;
        digits[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if value.negative != 0 {
        at -= 1;
        digits[at] = b'-';
    }
    runtime::from_bytes(&digits[at..])
}
