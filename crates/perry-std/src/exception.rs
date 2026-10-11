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

/// A thrown number; `TypeError` for any other thrown value.
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

/// A thrown string; `TypeError` for any other thrown value.
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
    from_static(match parts(exception).0 {
        TYPE_ERROR => "TypeError",
        2 => "RangeError",
        3 => "Error",
        _ => "",
    })
}
