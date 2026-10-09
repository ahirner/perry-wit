//! Exception objects (`crate::region::Task::exception`).
//!
//! Compiled code passes exceptions as an `i32` status (1 `TypeError`, 2 `RangeError`, 3
//! `Error`, 4 another thrown value, 5 `process.exit`). `throw` also makes an exception object
//! pending in the task, so `catch (e)` sees the thrown object itself; failures raised by
//! compiled code without an object (`perry.unwrap`, holes) get one materialized at the catch.
//!
//! Layout, read inline by compiled code: `kind: u32` at 0 (the status), `tag: u32` at 4,
//! `value: u64` at 8, `cause: u32` at 16. `Error` objects have tag [TAG_STRING] and their
//! message in `value`, and a string `cause` or null; other thrown values keep their slot
//! bits.
use crate::region::current;
use core::ptr;

pub const KIND: u32 = 0;
pub const TAG: u32 = 4;
pub const VALUE: u32 = 8;
pub const CAUSE: u32 = 16;

pub const TYPE_ERROR: u32 = 1;
pub const RANGE_ERROR: u32 = 2;
pub const ERROR: u32 = 3;
pub const THROWN_VALUE: u32 = 4;
pub const EXIT: u32 = 5;

pub const TAG_NUMBER: u32 = 1;
pub const TAG_BOOLEAN: u32 = 2;
pub const TAG_STRING: u32 = 3;

#[repr(C)]
struct Exception {
    kind: u32,
    tag: u32,
    value: u64,
    cause: *mut u8,
}

/// A new exception object in the current task.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exception_new(kind: u32, tag: u32, value: u64) -> *mut u8 {
    let exception = current().allocate(core::mem::size_of::<Exception>()) as *mut Exception;
    // SAFETY: freshly allocated.
    unsafe {
        exception.write(Exception {
            kind,
            tag,
            value,
            cause: ptr::null_mut(),
        })
    };
    exception as *mut u8
}

/// `throw exception`: make it pending and return its status.
///
/// # Safety
/// `exception` must be an exception object of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_exception_raise(exception: *mut u8) -> u32 {
    current().exception = exception;
    // SAFETY: guaranteed by the caller.
    unsafe { (*(exception as *mut Exception)).kind }
}

/// `catch`: take the pending exception, or materialize one for `status`.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exception_catch(status: u32) -> *mut u8 {
    let task = current();
    let pending = core::mem::replace(&mut task.exception, ptr::null_mut());
    if !pending.is_null() {
        return pending;
    }
    let message = match status {
        TYPE_ERROR => "Cannot read properties of undefined",
        RANGE_ERROR => "Invalid array index",
        _ => "",
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
    if (TYPE_ERROR..=ERROR).contains(&exception.kind) && exception.tag == TAG_STRING {
        exception.value as usize as *mut u8
    } else {
        crate::string::from_static("")
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
