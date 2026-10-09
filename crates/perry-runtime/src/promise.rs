//! Promises of the current task (design decision D5).
//!
//! Layout, read inline by compiled code: `state: u32` at 0 ([PENDING], [FULFILLED],
//! [REJECTED]), `status: i32` at 4 (the exception code of a rejection), the settled value's
//! 8-byte slot at 8 (`f64` bits or a 32-bit reference; a rejection's exception object). Compiled code checks `state` before
//! suspending, so awaiting a settled promise continues inline; otherwise the awaiting frame
//! becomes a waiter and is queued on the task's ready list when the promise settles.
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

/// Reject `promise` with exception code `status`, taking the pending exception object into
/// its value slot, and queue its waiters.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_reject(promise: *mut u8, status: i32) {
    let exception = core::mem::replace(&mut current().exception, ptr::null_mut());
    settle(promise, REJECTED, status, exception as usize as u64);
}

/// `await` of the rejected `promise`: make its exception pending again; returns its status.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_rethrow(promise: *mut u8) -> i32 {
    let promise = promise as *mut Promise;
    // SAFETY: compiled code passes a live, rejected promise.
    unsafe {
        current().exception = (*promise).value as usize as *mut u8;
        (*promise).status
    }
}

/// Queue `frame` when the pending `promise` settles.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_wait(promise: *mut u8, frame: *mut u8) {
    let promise = promise as *mut Promise;
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
}
