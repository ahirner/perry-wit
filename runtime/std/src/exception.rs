//! Thrown values used as typed values: `trace.push(outcome.reason)` after a numeric `throw`.
use crate::runtime::{
    EXCEPTION_KIND, EXCEPTION_TAG, EXCEPTION_VALUE, TAG_NUMBER, TAG_STRING, THROWN_VALUE,
    TYPE_ERROR, from_static, raise,
};

/// The kind, tag and value slot of an exception object.
fn parts(exception: *mut u8) -> (u32, u32, u64) {
    // SAFETY: compiled code passes exception objects.
    unsafe {
        (
            *(exception.add(EXCEPTION_KIND) as *const u32),
            *(exception.add(EXCEPTION_TAG) as *const u32),
            *(exception.add(EXCEPTION_VALUE) as *const u64),
        )
    }
}

/// Compare `Error` objects by identity and boxed thrown primitives by value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_equal(left: *mut u8, right: *mut u8) -> u32 {
    use perry_rt_sys::layout::exception::*;
    let (left_kind, left_tag, left_value) = parts(left);
    let (right_kind, right_tag, right_value) = parts(right);
    if left_kind != THROWN_VALUE || right_kind != THROWN_VALUE {
        return u32::from(left == right);
    }
    if left_tag != right_tag {
        return 0;
    }
    u32::from(match left_tag {
        TAG_NUMBER => f64::from_bits(left_value) == f64::from_bits(right_value),
        TAG_UNDEFINED | TAG_NULL => true,
        TAG_STRING => {
            // SAFETY: both payloads are live strings.
            unsafe {
                crate::runtime::bytes(left_value as usize as *mut u8)
                    == crate::runtime::bytes(right_value as usize as *mut u8)
            }
        }
        TAG_BIGINT => {
            // SAFETY: both payloads are live integers.
            unsafe {
                crate::integers::std_bigint_compare(
                    left_value as usize as *const _,
                    right_value as usize as *const _,
                ) == 0
            }
        }
        TAG_JSON => {
            crate::json::std_json_strict_equal(
                left_value as usize as *mut u8,
                right_value as usize as *mut u8,
            ) != 0
        }
        _ => left_value == right_value,
    })
}

/// Compare a caught value with an integer without coercing other thrown values.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_bigint_equal(exception: *mut u8, value: *const u8) -> u32 {
    use perry_rt_sys::layout::exception::TAG_BIGINT;
    match parts(exception) {
        (THROWN_VALUE, TAG_BIGINT, integer) => {
            // SAFETY: the tag proves the payload is a live integer; compiled code supplies value.
            u32::from(unsafe {
                crate::integers::std_bigint_compare(integer as usize as *const _, value.cast()) == 0
            })
        }
        _ => 0,
    }
}

/// Extract a thrown number or raise `TypeError` for any other thrown value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_to_number(exception: *mut u8) -> f64 {
    match parts(exception) {
        (THROWN_VALUE, TAG_NUMBER, bits) => f64::from_bits(bits),
        _ => {
            raise(TYPE_ERROR, "Thrown value is not a number");
            0.0
        }
    }
}

/// Extract a thrown string or raise `TypeError` for any other thrown value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_to_string(exception: *mut u8) -> *mut u8 {
    match parts(exception) {
        (THROWN_VALUE, TAG_STRING, value) => value as usize as *mut u8,
        _ => {
            raise(TYPE_ERROR, "Thrown value is not a string");
            from_static("")
        }
    }
}

/// `error.name`: the constructor's name of an `Error` object (thrown values have none).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_name(exception: *mut u8) -> *mut u8 {
    use perry_rt_sys::layout::exception::{ERROR, RANGE_ERROR, REFERENCE_ERROR, SYNTAX_ERROR};
    from_static(match parts(exception).0 {
        TYPE_ERROR => "TypeError",
        RANGE_ERROR => "RangeError",
        ERROR => "Error",
        REFERENCE_ERROR => "ReferenceError",
        SYNTAX_ERROR => "SyntaxError",
        _ => "",
    })
}

/// Extract a thrown boolean or raise a checked `TypeError` for any other thrown value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_exception_to_bool(exception: *mut u8) -> u32 {
    use perry_rt_sys::layout::exception::TAG_BOOLEAN;
    match parts(exception) {
        (THROWN_VALUE, TAG_BOOLEAN, value) => u32::from(value != 0),
        _ => {
            raise(TYPE_ERROR, "Thrown value is not a boolean");
            0
        }
    }
}
