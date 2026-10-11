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

const PENDING: u32 = 0;
pub const FULFILLED: u32 = 1;
pub const REJECTED: u32 = 2;

#[repr(C)]
struct Promise {
    state: u32,
    status: i32,
    value: u64,
    waiters: *mut Node,
    owner: *mut u8,
    references: bool,
}

/// A pending promise in the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_new() -> *mut u8 {
    perry_promise_new_typed(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_new_typed(references: u32) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        perry_promise_new_collectible(references)
    }
    #[cfg(not(feature = "collectible"))]
    {
        let promise = current().allocate(core::mem::size_of::<Promise>()) as *mut Promise;
        // SAFETY: freshly allocated.
        unsafe { pending(promise, references) }
    }
}

/// A typed promise whose settled reference payload is traced precisely.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_new_collectible(references: u32) -> *mut u8 {
    static SHAPE: [u32; 3] = [0, 1, core::mem::offset_of!(Promise, owner) as u32];
    // SAFETY: the static shape describes the owner field, independently of value-slot bits.
    let promise = unsafe {
        crate::roots::perry_gc_alloc(core::mem::size_of::<Promise>() as u32, SHAPE.as_ptr())
    } as *mut Promise;
    // SAFETY: freshly allocated; allocation never collects within this helper.
    unsafe { pending(promise, references) }
}

/// Initialize fresh storage as a pending promise; `references` types its settled value.
///
/// # Safety
/// `promise` points to writable storage for a [Promise].
unsafe fn pending(promise: *mut Promise, references: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        promise.write(Promise {
            state: PENDING,
            status: 0,
            value: 0,
            waiters: ptr::null_mut(),
            owner: ptr::null_mut(),
            references: references != 0,
        })
    };
    promise.cast()
}

pub(crate) unsafe fn has_reference_value(promise: *mut u8) -> bool {
    // SAFETY: caller supplies a live promise.
    unsafe { (*(promise as *const Promise)).references }
}

/// `promise` made pending again if it settled and nothing waits on it, otherwise a new
/// promise. A leased operation (`perry_codegen::lease`) passes its previous promise, which no
/// code can observe any more.
#[unsafe(no_mangle)]
pub extern "C" fn perry_promise_reuse(promise: *mut u8) -> *mut u8 {
    let reusable = promise as *mut Promise;
    // SAFETY: a non-null argument is a live promise of the current task.
    unsafe {
        if reusable.is_null() || (*reusable).state == PENDING || !(*reusable).waiters.is_null() {
            return perry_promise_new_typed(1);
        }
        (*reusable).state = PENDING;
        (*reusable).status = 0;
        (*reusable).value = 0;
        (*reusable).owner = ptr::null_mut();
    }
    promise
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
        (*promise).owner = if state == REJECTED || (*promise).references {
            value as usize as *mut u8
        } else {
            ptr::null_mut()
        };
        let mut waiter = (*promise).waiters;
        (*promise).waiters = ptr::null_mut();
        while !waiter.is_null() {
            let next = (*waiter).next;
            task.push_ready_node(waiter);
            waiter = next;
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
pub(crate) extern "C" fn perry_promise_rethrow(promise: *mut u8) -> i32 {
    let promise = promise as *mut Promise;
    // SAFETY: compiled code passes a live, rejected promise.
    unsafe {
        current().exception = (*promise).value as usize as *mut u8;
        (*promise).status
    }
}

/// Queue `frame` when the pending `promise` settles.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_wait(promise: *mut u8, frame: *mut u8) {
    let promise = promise as *mut Promise;
    let node = current().node(frame);
    // SAFETY: `promise` is live; `node` is ours. Waiters run in registration order.
    unsafe {
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
const ALL_SETTLED: u32 = 2;

/// A `Promise.all`/`race`/`allSettled` in progress: a frame (kind first) that waits on every
/// pending input at once and re-checks whenever one settles.
#[repr(C)]
struct Combinator {
    kind: u32,
    mode: u32,
    result: *mut u8,
    inputs: *mut u8,
    registered: u32,
    pin: u32,
    references: bool,
}

fn input(inputs: *mut u8, index: u32) -> *mut Promise {
    // SAFETY: an array of promises (layout of crate::array).
    unsafe {
        let data = *(inputs.add(crate::array::DATA as usize) as *const *const u64);
        *data.add(index as usize) as usize as *mut Promise
    }
}

fn combine(mode: u32, inputs: *mut u8, collectible: bool, references: bool) -> *mut u8 {
    let result = if collectible {
        perry_promise_new_collectible(u32::from(mode != RACE || references))
    } else {
        perry_promise_new()
    };
    let size = core::mem::size_of::<Combinator>();
    let frame = if collectible {
        static SHAPE: [u32; 4] = [
            0,
            2,
            core::mem::offset_of!(Combinator, result) as u32,
            core::mem::offset_of!(Combinator, inputs) as u32,
        ];
        // SAFETY: exact pointer map of the frame.
        unsafe { crate::roots::perry_gc_alloc(size as u32, SHAPE.as_ptr()) }
    } else {
        current().allocate(size)
    } as *mut Combinator;
    let pin = if collectible {
        crate::roots::perry_gc_pin(frame.cast())
    } else {
        0
    };
    // SAFETY: freshly allocated.
    unsafe {
        frame.write(Combinator {
            kind: COMBINATOR_KIND,
            mode,
            result,
            inputs,
            registered: 0,
            pin,
            references,
        })
    };
    perry_combinator_step(frame as *mut u8);
    result
}

/// `Promise.all(inputs)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_all(inputs: *mut u8) -> *mut u8 {
    combine(ALL, inputs, false, false)
}

/// `Promise.race(inputs)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_race(inputs: *mut u8) -> *mut u8 {
    combine(RACE, inputs, false, false)
}

/// `Promise.allSettled(inputs)`: fulfils with the settled inputs themselves, which compiled
/// code reads as `{ status, value, reason }` (the promise layout above).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_all_settled(inputs: *mut u8) -> *mut u8 {
    combine(ALL_SETTLED, inputs, false, false)
}

/// Typed combinator; `references` describes fulfilled input payloads.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_promise_combine_collectible(
    mode: u32,
    inputs: *mut u8,
    references: u32,
) -> *mut u8 {
    combine(mode, inputs, true, references != 0)
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
pub(crate) extern "C" fn perry_combinator_step(frame: *mut u8) {
    let frame = frame as *mut Combinator;
    // SAFETY: guaranteed by the caller; inputs are live promises of the task.
    unsafe {
        let combinator = &mut *frame;
        let count = *(combinator.inputs as *const u32);
        let pending = (0..count).any(|index| (*input(combinator.inputs, index)).state == PENDING);
        if (*(combinator.result as *mut Promise)).state == PENDING {
            for index in 0..count {
                let promise = input(combinator.inputs, index);
                match ((*promise).state, combinator.mode) {
                    (REJECTED, ALL | RACE) => {
                        reject_with(combinator.result, promise);
                        break;
                    }
                    (FULFILLED, RACE) => {
                        perry_promise_resolve(combinator.result, (*promise).value);
                        break;
                    }
                    _ => {}
                }
            }
            if !pending
                && combinator.mode != RACE
                && (*(combinator.result as *mut Promise)).state == PENDING
            {
                let values = if combinator.pin != 0 {
                    crate::array::perry_array_new_collectible(
                        count,
                        u32::from(combinator.mode == ALL_SETTLED || combinator.references),
                    )
                } else {
                    crate::array::perry_array_new(count)
                };
                let data = *(values.add(crate::array::DATA as usize) as *const *mut u64);
                for index in 0..count {
                    let promise = input(combinator.inputs, index);
                    *data.add(index as usize) = if combinator.mode == ALL_SETTLED {
                        promise as usize as u64
                    } else {
                        (*promise).value
                    };
                }
                *(values as *mut u32) = count;
                perry_promise_resolve(combinator.result, values as usize as u64);
            }
        }
        let settled = (*(combinator.result as *mut Promise)).state != PENDING;
        if settled && (combinator.registered == 0 || !pending) {
            if combinator.pin != 0 {
                crate::roots::perry_gc_unpin(combinator.pin);
                combinator.pin = 0;
            }
            return;
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
