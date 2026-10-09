//! Promises of the current task (design decision D5).
//!
//! Layout, read inline by compiled code: `state: u32` at 0 ([PENDING], [FULFILLED],
//! [REJECTED]), `status: i32` at 4 (the exception code of a rejection), the settled value's
//! 8-byte slot at 8 (`f64` bits or a 32-bit reference; an exception reference on rejection).
//! Uncontended settled awaits continue inline. Pending promises retain waiters; ready
//! competing work gets a turn through the task queue, without a synthetic microtask contract.
use crate::region::{Node, current};
use core::ptr;

pub const STATE: u32 = 0;
pub const STATUS: u32 = 4;
pub const VALUE: u32 = 8;

pub const PENDING: u32 = 0;
pub const FULFILLED: u32 = 1;
pub const REJECTED: u32 = 2;

#[repr(C)]
struct Promise {
    state: u32,
    status: i32,
    value: u64,
    waiters: *mut Node,
    observed: bool,
    next_rejected: *mut Promise,
}

/// A pending promise in the current task.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_new() -> *mut u8 {
    let promise = current().allocate(core::mem::size_of::<Promise>()) as *mut Promise;
    // SAFETY: freshly allocated.
    unsafe {
        promise.write(Promise {
            state: PENDING,
            status: 0,
            value: 0,
            waiters: ptr::null_mut(),
            observed: false,
            next_rejected: ptr::null_mut(),
        })
    };
    promise as *mut u8
}

fn settle(promise: *mut u8, state: u32, status: i32, value: u64) {
    let promise = promise as *mut Promise;
    let task = current();
    // SAFETY: compiled code passes live promises of the current task.
    unsafe {
        if (*promise).state != PENDING {
            return;
        }
        (*promise).state = state;
        (*promise).status = status;
        (*promise).value = value;
        let mut waiter = (*promise).waiters;
        (*promise).waiters = ptr::null_mut();
        while !waiter.is_null() {
            task.push_ready((*waiter).frame);
            waiter = (*waiter).next;
        }
    }
}

/// Fulfil `promise` with the slot bits `value` and queue its waiters.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_resolve(promise: *mut u8, value: u64) {
    settle(promise, FULFILLED, 0, value);
}

/// Consume the pending exception into `promise` and queue its waiters.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_reject(promise: *mut u8, status: i32) {
    let exception = crate::exception::perry_exception_catch(status);
    // SAFETY: compiled code passes a live promise of the current task.
    unsafe {
        if (*(promise as *const Promise)).state != PENDING {
            return;
        }
    }
    settle(promise, REJECTED, status, exception as usize as u64);
    let task = current();
    // SAFETY: each promise is linked once, when it first rejects.
    unsafe { (*(promise as *mut Promise)).next_rejected = task.rejections.cast() };
    task.rejections = promise;
}

/// Register a rejection handler when an await starts, even if the promise already settled.
///
/// # Safety
/// `promise` is a live promise of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_promise_observe(promise: *mut u8) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*(promise as *mut Promise)).observed = true };
}

/// Check failures at the end of a job, after all ready continuations ran. An observed
/// rejection never becomes unobserved, so discard checked entries to keep total work linear.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_failed() -> u32 {
    if current().output_failed {
        return 1;
    }
    let mut promise =
        core::mem::replace(&mut current().rejections, ptr::null_mut()) as *const Promise;
    // SAFETY: the rejection list belongs to the current live task.
    unsafe {
        while !promise.is_null() {
            if !(*promise).observed {
                return 1;
            }
            promise = (*promise).next_rejected;
        }
    }
    0
}

/// Restore the original rejection, including its identity, for an `await`.
///
/// # Safety
/// `promise` is a live rejected promise of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_promise_rethrow(promise: *mut u8) -> i32 {
    // SAFETY: guaranteed by the caller; rejected promises store an exception reference.
    unsafe {
        let promise = &*(promise as *const Promise);
        crate::exception::perry_exception_raise(promise.value as usize as *mut u8)
    }
}

/// Return zero for an uncontended settled await, otherwise queue/register `frame` and return one.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_wait(promise: *mut u8, frame: *mut u8) -> u32 {
    let promise = promise as *mut Promise;
    // SAFETY: compiled code passes a live promise of the current task.
    if unsafe { (*promise).state != PENDING } {
        if current().ready_head.is_null() {
            return 0;
        }
        current().push_ready(frame);
        return 1;
    }
    let node = current().allocate(core::mem::size_of::<Node>()) as *mut Node;
    // SAFETY: `promise` is live; `node` is fresh. Waiters run in registration order.
    unsafe {
        node.write(Node {
            frame,
            next: ptr::null_mut(),
        });
        let mut link: *mut *mut Node = &mut (*promise).waiters;
        while !(*link).is_null() {
            link = &mut (**link).next;
        }
        *link = node;
    }
    1
}
