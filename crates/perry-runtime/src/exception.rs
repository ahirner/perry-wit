//! Exception values belong to the task. A pending exception is consumed at a catch or
//! promise rejection; retaining the value itself preserves identity through rethrows.
use crate::{region::current, string::perry_string_adopt};
use core::ptr;

pub const TYPE_ERROR: u32 = 1;
pub const RANGE_ERROR: u32 = 2;
pub const ERROR: u32 = 3;
pub const NUMBER: u32 = 4;
pub const BOOLEAN: u32 = 5;
pub const STRING: u32 = 6;

pub(crate) fn raise_error(kind: u32, message: &'static str) -> i32 {
    // SAFETY: literal bytes have static lifetime; the exception is owned by this task.
    unsafe {
        let text = perry_string_adopt(message.as_ptr().cast_mut(), message.len() as u32);
        let error = perry_exception_new(kind, text as usize as u64);
        perry_exception_raise(error)
    }
}

#[repr(C)]
struct Exception {
    kind: u32,
    value: u64,
}

/// Allocate an Error (string payload) or a thrown primitive (typed slot bits).
#[unsafe(no_mangle)]
pub extern "C" fn perry_exception_new(kind: u32, value: u64) -> *mut u8 {
    let exception = current().allocate(core::mem::size_of::<Exception>()) as *mut Exception;
    // SAFETY: freshly allocated in the current task.
    unsafe { exception.write(Exception { kind, value }) };
    exception.cast()
}

/// Set the current task's pending exception, returning its nonzero status.
///
/// # Safety
/// `exception` is a live exception value in the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_raise(exception: *mut u8) -> i32 {
    current().exception = exception;
    // SAFETY: guaranteed by the caller.
    unsafe { (*(exception as *const Exception)).kind as i32 }
}

/// Consume the pending exception, materializing an implicit runtime failure if necessary.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exception_catch(status: i32) -> *mut u8 {
    let pending = core::mem::replace(&mut current().exception, ptr::null_mut());
    if !pending.is_null() {
        return pending;
    }
    let message = match status as u32 {
        TYPE_ERROR => "value is undefined",
        RANGE_ERROR => "array index is outside the dense range",
        _ => "runtime error",
    };
    // SAFETY: the literal bytes live for the instance lifetime.
    let message = unsafe { perry_string_adopt(message.as_ptr().cast_mut(), message.len() as u32) };
    perry_exception_new(status as u32, message as usize as u64)
}

/// `instanceof Error`, `TypeError` or `RangeError`.
///
/// # Safety
/// `exception` is a live exception value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_is(exception: *mut u8, expected: u32) -> u32 {
    // SAFETY: guaranteed by the caller.
    let kind = unsafe { (*(exception as *const Exception)).kind };
    u32::from(if expected == ERROR {
        (TYPE_ERROR..=ERROR).contains(&kind)
    } else {
        kind == expected
    })
}

/// The message of an Error; primitive throws have no message property.
///
/// # Safety
/// `exception` is a live exception value.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_message(exception: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let exception = unsafe { &*(exception as *const Exception) };
    if (TYPE_ERROR..=ERROR).contains(&exception.kind) {
        exception.value as usize as *mut u8
    } else {
        ptr::null_mut()
    }
}

/// Strict equality of thrown values: Error identity and primitive value equality.
///
/// # Safety
/// Both operands are live exception values.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_equal(a: *mut u8, b: *mut u8) -> u32 {
    // SAFETY: guaranteed by the caller.
    let (left, right) = unsafe { (&*(a as *const Exception), &*(b as *const Exception)) };
    if left.kind != right.kind {
        return 0;
    }
    match left.kind {
        NUMBER => u32::from(f64::from_bits(left.value) == f64::from_bits(right.value)),
        BOOLEAN => u32::from(left.value == right.value),
        // SAFETY: STRING payloads are live string references.
        STRING => unsafe {
            crate::string::perry_string_equal(
                left.value as usize as *mut u8,
                right.value as usize as *mut u8,
            )
        },
        _ => u32::from(a == b),
    }
}
