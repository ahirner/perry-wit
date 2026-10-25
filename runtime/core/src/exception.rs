//! Exception objects (`crate::region::Task::exception`).
//!
//! Compiled code passes exceptions as an `i32` status (1 `TypeError`, 2 `RangeError`, 3
//! `Error`, 4 another thrown value, 5 `process.exit`, 6 `ReferenceError`, 7 `SyntaxError`).
//! A `throw` statement also records a pending exception object on the task so `catch (e)`
//! receives the thrown value directly. Failures raised by compiled checks such as
//! `perry.unwrap` or array holes materialize an exception object when caught.
//!
//! Layout, read inline by compiled code: `kind: u32` at 0 (the status), `tag: u32` at 4,
//! `value: u64` at 8, `cause: u32` at 16, `owner: u32` at 20. `Error` objects have tag
//! [TAG_STRING] and their message in `value`, with a JSON value or null in `cause`.
//! Other thrown values keep their slot bits. The owner retains referenced payloads.
use crate::region::current;
use core::ptr;

pub(crate) use crate::layout::exception::{
    ERROR, KIND, RANGE_ERROR, REFERENCE_ERROR, SYNTAX_ERROR, TAG, TAG_BIGINT, TAG_BOOLEAN,
    TAG_JSON, TAG_NULL, TAG_NUMBER, TAG_STRING, TAG_UNDEFINED, TYPE_ERROR, VALUE,
};

pub(crate) const EXIT: u32 = 5;

#[repr(C)]
struct Exception {
    kind: u32,
    tag: u32,
    value: u64,
    cause: *mut u8,
    owner: *mut u8,
}

/// A new exception object in the current task.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exception_new(kind: u32, tag: u32, value: u64) -> *mut u8 {
    static SHAPE: [u32; 4] = [
        0,
        2,
        core::mem::offset_of!(Exception, cause) as u32,
        core::mem::offset_of!(Exception, owner) as u32,
    ];
    // SAFETY: the map lists only explicit reference fields.
    let exception = unsafe {
        crate::region::perry_alloc_shaped(core::mem::size_of::<Exception>() as u32, SHAPE.as_ptr())
    } as *mut Exception;
    // SAFETY: freshly allocated.
    unsafe {
        exception.write(Exception {
            kind,
            tag,
            value,
            cause: ptr::null_mut(),
            owner: if matches!(tag, TAG_STRING | TAG_BIGINT | TAG_JSON) {
                value as usize as *mut u8
            } else {
                ptr::null_mut()
            },
        })
    };
    exception as *mut u8
}

/// `throw exception`: make it pending and return its status.
///
/// # Safety
/// `exception` must be an exception object of the current task.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_exception_raise(exception: *mut u8) -> u32 {
    current().exception = exception;
    // SAFETY: guaranteed by the caller.
    unsafe { (*(exception as *mut Exception)).kind }
}

/// `catch`: take the pending exception, or materialize one for `status`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_exception_catch(status: u32) -> *mut u8 {
    let task = current();
    let pending = core::mem::replace(&mut task.exception, ptr::null_mut());
    if !pending.is_null() {
        return pending;
    }
    let message = match status {
        TYPE_ERROR => perry_abi::static_text!("Cannot read properties of undefined"),
        RANGE_ERROR => perry_abi::static_text!("Invalid array index"),
        _ => perry_abi::static_text!(""),
    };
    let message = crate::string::from_static(message);
    perry_exception_new(status, TAG_STRING, message as usize as u64)
}

/// `e.message`: the message of an `Error`, the empty string for other thrown values.
///
/// # Safety
/// `exception` must be an exception object of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_message(exception: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let exception = unsafe { &*(exception as *mut Exception) };
    if matches!(
        exception.kind,
        TYPE_ERROR..=ERROR | REFERENCE_ERROR | SYNTAX_ERROR
    ) && exception.tag == TAG_STRING
    {
        exception.value as usize as *mut u8
    } else {
        crate::string::from_static(perry_abi::static_text!(""))
    }
}

/// Throw an `Error` of `kind` with the string `message` from a runtime service (the
/// built-ins of `perry-std`): the exception becomes pending, and compiled code finds its
/// status with [perry_status].
#[unsafe(no_mangle)]
pub extern "C" fn perry_raise(kind: u32, message: *mut u8) {
    let exception = perry_exception_new(kind, TAG_STRING, message as usize as u64);
    let task = current();
    task.exception = exception;
    task.status = kind;
}

/// The status of the last runtime service call (0 if it did not throw), clearing it.
#[unsafe(no_mangle)]
pub extern "C" fn perry_status() -> u32 {
    core::mem::replace(&mut current().status, 0)
}

/// Replace a stale handled exception when compiled checks raise a fresh implicit failure.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_exception_implicit(status: u32) -> u32 {
    current().exception = ptr::null_mut();
    status
}

/// Raise a native failure with a structured cause retained by the exception's pointer map.
///
/// # Safety
/// The message is a live string and cause is a live JSON graph or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_raise_with_cause(kind: u32, message: *mut u8, cause: *mut u8) {
    perry_raise(kind, message);
    unsafe {
        (*current().exception.cast::<Exception>()).cause = cause;
    }
}
