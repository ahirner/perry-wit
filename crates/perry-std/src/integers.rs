//! Immutable signed-magnitude values for exact WIT integer transport.
use crate::runtime;

/// Native and guest layout is identical; no pointer fields are stored here.
#[repr(C)]
pub(crate) struct Integer {
    pub(crate) magnitude: u64,
    pub(crate) negative: u32,
    padding: u32,
}

#[cfg(target_arch = "wasm32")]
const PARSED_INT_COOKIE: u32 = 0x5049_4e54;
const PARSED_INT_TAG: u64 = 0x9_5049;

#[cfg(not(target_arch = "wasm32"))]
fn host_table() -> &'static std::sync::Mutex<std::vec::Vec<u64>> {
    static TABLE: std::sync::OnceLock<std::sync::Mutex<std::vec::Vec<u64>>> =
        std::sync::OnceLock::new();
    TABLE.get_or_init(|| std::sync::Mutex::new(std::vec::Vec::new()))
}

/// Encode a 64-bit integer parsed by `parseInt` as an `f64`, preserving exact 64-bit
/// precision across `f64` value boundaries when more than 53 significant bits are present.
pub(crate) fn encode_parsed_int(negative: bool, magnitude: u64) -> f64 {
    if magnitude == 0 {
        return if negative { -0.0 } else { 0.0 };
    }
    if 64 - magnitude.leading_zeros() - magnitude.trailing_zeros() <= 53 {
        let value = magnitude as f64;
        return if negative { -value } else { value };
    }
    #[cfg(target_arch = "wasm32")]
    let handle = {
        let ptr = runtime::alloc(16).cast::<Integer>();
        unsafe {
            ptr.write(Integer {
                magnitude,
                negative: u32::from(negative),
                padding: PARSED_INT_COOKIE,
            });
        }
        ptr as usize as u32
    };
    #[cfg(not(target_arch = "wasm32"))]
    let handle = {
        let mut table = host_table().lock().unwrap_or_else(|e| e.into_inner());
        let index = table.len() as u32 + 1;
        table.push(magnitude);
        index
    };
    let sign = u64::from(negative) << 63;
    let exp = u64::from(1023 + (63 - magnitude.leading_zeros()));
    f64::from_bits(sign | (exp << 52) | (PARSED_INT_TAG << 32) | u64::from(handle))
}

/// Recover the exact `(negative, magnitude)` of a 64-bit integer produced by `parseInt`.
pub(crate) fn exact_parsed_int(value: f64) -> Option<(bool, u64)> {
    let bits = value.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32 - 1023;
    if !(53..=63).contains(&exp) || ((bits >> 32) & 0xfffff) != PARSED_INT_TAG {
        return None;
    }
    let negative = (bits >> 63) != 0;
    #[cfg(target_arch = "wasm32")]
    {
        let ptr = (bits as u32) as usize as *const Integer;
        if ptr.is_null() || !(ptr as usize).is_multiple_of(8) {
            return None;
        }
        let integer = unsafe { &*ptr };
        if integer.padding != PARSED_INT_COOKIE {
            return None;
        }
        Some((negative, integer.magnitude))
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let index = (bits as u32).checked_sub(1)? as usize;
        let table = host_table().lock().unwrap_or_else(|e| e.into_inner());
        table.get(index).copied().map(|mag| (negative, mag))
    }
}

/// Convert an integral finite number within the signed-magnitude transport domain.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_bigint_from_number(value: f64) -> *mut Integer {
    if let Some((negative, magnitude)) = exact_parsed_int(value) {
        let result = runtime::alloc(16).cast::<Integer>();
        unsafe {
            result.write(Integer {
                magnitude,
                negative: u32::from(negative),
                padding: 0,
            });
        }
        return result;
    }
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
