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

/// Frame kind of runtime combinators; `drive` passes such frames to [perry_combinator_step].
pub const COMBINATOR_KIND: u32 = u32::MAX;

const ALL: u32 = 0;
const RACE: u32 = 1;

/// A `Promise.all`/`Promise.race` in progress: a frame (kind first) that waits on every
/// pending input at once and re-checks whenever one settles.
#[repr(C)]
struct Combinator {
    kind: u32,
    mode: u32,
    result: *mut u8,
    inputs: *mut u8,
    registered: u32,
}

fn input(inputs: *mut u8, index: u32) -> *mut Promise {
    // SAFETY: an array of promises (layout of crate::array).
    unsafe {
        let data = *(inputs.add(crate::array::DATA as usize) as *const *const u64);
        *data.add(index as usize) as usize as *mut Promise
    }
}

fn combine(mode: u32, inputs: *mut u8) -> *mut u8 {
    let result = perry_promise_new();
    let frame = current().allocate(core::mem::size_of::<Combinator>()) as *mut Combinator;
    // SAFETY: freshly allocated.
    unsafe {
        frame.write(Combinator {
            kind: COMBINATOR_KIND,
            mode,
            result,
            inputs,
            registered: 0,
        })
    };
    perry_combinator_step(frame as *mut u8);
    result
}

/// `Promise.all(inputs)`.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_all(inputs: *mut u8) -> *mut u8 {
    combine(ALL, inputs)
}

/// `Promise.race(inputs)`.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_race(inputs: *mut u8) -> *mut u8 {
    combine(RACE, inputs)
}

fn reject_with(result: *mut u8, input: *mut Promise) {
    // SAFETY: a rejected input keeps its exception object in its value slot.
    unsafe {
        current().exception = (*input).value as usize as *mut u8;
        perry_promise_reject(result, (*input).status);
    }
}

/// Re-check a combinator after an input settled (or on creation).
///
/// # Safety
/// `frame` must be a combinator frame.
#[unsafe(no_mangle)]
pub extern "C" fn perry_combinator_step(frame: *mut u8) {
    let frame = frame as *mut Combinator;
    // SAFETY: guaranteed by the caller; inputs are live promises of the task.
    unsafe {
        let combinator = &mut *frame;
        if (*(combinator.result as *mut Promise)).state != PENDING {
            return;
        }
        let count = *(combinator.inputs as *const u32);
        let mut pending = false;
        for index in 0..count {
            let promise = input(combinator.inputs, index);
            match ((*promise).state, combinator.mode) {
                (REJECTED, _) => return reject_with(combinator.result, promise),
                (FULFILLED, RACE) => {
                    return perry_promise_resolve(combinator.result, (*promise).value);
                }
                (PENDING, _) => pending = true,
                _ => {}
            }
        }
        if !pending {
            // `all`: every input fulfilled; collect the values in input order.
            let values = crate::array::perry_array_new(count);
            let data = *(values.add(crate::array::DATA as usize) as *const *mut u64);
            for index in 0..count {
                *data.add(index as usize) = (*input(combinator.inputs, index)).value;
            }
            *(values as *mut u32) = count;
            return perry_promise_resolve(combinator.result, values as usize as u64);
        }
        if combinator.registered == 0 {
            combinator.registered = 1;
            for index in 0..count {
                let promise = input(combinator.inputs, index);
                if (*promise).state == PENDING {
                    perry_promise_wait(promise as *mut u8, frame as *mut u8);
                }
            }
        }
    }
}
