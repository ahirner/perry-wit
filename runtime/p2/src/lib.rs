//! Cooperative P2 scheduler. Only `wasi:io/poll` crosses the component boundary.
#![no_std]
extern crate alloc;
use alloc::{boxed::Box, vec::Vec};
use core::cell::UnsafeCell;
use perry_p2_sys::{BLOCKED, EVENT_FUTURE_READ, EVENT_SUBTASK, Operation, wasip2};

struct State {
    task: u32,
    operations: Vec<Option<Box<Operation>>>,
    event: [u32; 3],
}
struct Global(UnsafeCell<State>);
// SAFETY: one P2 component instance executes one synchronous export at a time.
unsafe impl Sync for Global {}
static STATE: Global = Global(UnsafeCell::new(State {
    task: 0,
    operations: Vec::new(),
    event: [0; 3],
}));
fn state() -> &'static mut State {
    unsafe { &mut *STATE.0.get() }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_context_set(task: u32) {
    state().task = task;
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_context_get() -> u32 {
    state().task
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_register(op: *mut Operation) -> u32 {
    let operation = unsafe { Box::from_raw(op) };
    // Never reuse an ID during a call: stale completion handles cannot alias a new operation.
    state().operations.push(Some(operation));
    state().operations.len() as u32
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_get(handle: u32) -> *mut Operation {
    state()
        .operations
        .get_mut(handle.checked_sub(1).expect("null waitable") as usize)
        .and_then(Option::as_deref_mut)
        .expect("live waitable") as *mut _
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_drop(handle: u32) {
    if handle == 0 {
        return;
    }
    let operation = unsafe { &mut *p2_get(handle) };
    operation.refs -= 1;
    if operation.refs == 0 {
        let removed = state().operations[(handle - 1) as usize].take();
        drop(removed);
    }
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_close() {
    // Drop backend resources and pinned Rust allocations before the guest task is freed.
    let operations = core::mem::take(&mut state().operations);
    drop(operations);
    state().task = 0;
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_set_new() -> u32 {
    1
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_set_drop(_: u32) {}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_join(handle: u32, set: u32) {
    unsafe { (*p2_get(handle)).set = set };
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_future_new() -> u64 {
    let mut operation = Operation::new(EVENT_FUTURE_READ, 0);
    operation.refs = 2;
    let id = unsafe { p2_register(Box::into_raw(Box::new(operation))) };
    u64::from(id) | (u64::from(id) << 32)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_future_read(handle: u32, result: *mut u8) -> u32 {
    let operation = unsafe { &mut *p2_get(handle) };
    operation.result = result;
    if operation.ready {
        unsafe { perry_p2_sys::finish(operation) };
        operation.notified = true;
        0
    } else {
        BLOCKED
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_future_write(handle: u32, _: *const u8) -> u32 {
    let operation = unsafe { &mut *p2_get(handle) };
    unsafe { perry_p2_sys::finish(operation) };
    0
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_cancel(handle: u32) -> u32 {
    let operation = unsafe { &mut *p2_get(handle) };
    operation.set = 0;
    operation.notified = true;
    drop(operation.wait.take());
    0
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_subtask_cancel(handle: u32) -> u32 {
    let returned = unsafe { (*p2_get(handle)).ready };
    p2_cancel(handle);
    if returned { 2 } else { 3 }
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_task_cancel() {
    panic!("P2 exports cannot receive native task cancellation")
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_poll(set: u32) -> *const u32 {
    unsafe { perry_p2_sys::task_resume(state().task as *mut u8) };
    loop {
        for index in 0..state().operations.len() {
            let Some(operation) = state().operations[index].as_deref_mut() else {
                continue;
            };
            if operation.set != set {
                continue;
            }
            // Box keeps the operation stable if its backend registers another operation.
            let operation = operation as *mut Operation;
            unsafe {
                if !(*operation).ready
                    && let Some(step) = (*operation).step
                {
                    step(operation);
                }
                if (*operation).ready && !(*operation).notified {
                    (*operation).notified = true;
                    state().event = [(*operation).event, index as u32 + 1, (*operation).code];
                    return state().event.as_ptr();
                }
            }
        }
        let pollables: Vec<_> = state()
            .operations
            .iter()
            .flatten()
            .filter(|op| op.set == set && !op.ready)
            .filter_map(|op| op.wait.as_ref())
            .collect();
        assert!(
            !pollables.is_empty(),
            "pending P2 task has no pollable or ready promise"
        );
        wasip2::io::poll::poll(&pollables);
    }
}

unsafe extern "C" fn timer_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    if operation.wait.as_ref().unwrap().ready() {
        unsafe { perry_p2_sys::finish(operation) };
    }
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_wait_for(duration: u64) -> u32 {
    let mut operation = Operation::new(EVENT_SUBTASK, 2);
    operation.wait = Some(wasip2::clocks::monotonic_clock::subscribe_duration(
        duration,
    ));
    operation.step = Some(timer_step);
    let id = unsafe { p2_register(Box::into_raw(Box::new(operation))) };
    id << 4
}
