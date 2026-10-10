//! Promise-timer cancellation. Active scheduler entries watch a collectible signal and
//! unlink on completion, cancellation or task cleanup; completed timers retain no history.
use crate::{exception, host, promise, region};
use core::ptr;

#[repr(C)]
pub(crate) struct Signal {
    aborted: u32,
    head: *mut region::Subtask,
}

/// `new AbortController()`. Scheduler entries root signals while they are watched.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_abort_new() -> *mut u8 {
    let signal = region::perry_alloc(core::mem::size_of::<Signal>() as u32).cast::<Signal>();
    // SAFETY: fresh storage; head refers to task-owned scheduler nodes, not collectible objects.
    unsafe {
        signal.write(Signal {
            aborted: 0,
            head: ptr::null_mut(),
        });
    }
    signal.cast()
}

/// `signal.aborted`.
///
/// # Safety
/// `signal` is a live signal.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_abort_aborted(signal: *mut u8) -> u32 {
    unsafe { (*signal.cast::<Signal>()).aborted }
}

/// Reject a timer with the accepted default abort reason.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_abort_reject(promise: *mut u8, message: *mut u8) {
    let error = exception::perry_exception_new(
        exception::ERROR,
        exception::TAG_STRING,
        message as usize as u64,
    );
    // SAFETY: fresh exception object; no collection occurs inside this helper.
    let status = unsafe { exception::perry_exception_raise(error) };
    promise::perry_promise_reject(promise, status as i32);
}

/// Watch a registered timer without allocating a second scheduler node.
///
/// # Safety
/// `signal` is live; `handle` and `promise` identify a newly registered timer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_abort_watch(signal: *mut u8, handle: u32, promise: *mut u8) {
    let signal = signal.cast::<Signal>();
    let mut entry = region::current().subtasks;
    unsafe {
        while !entry.is_null() {
            if (*entry).handle == handle && (*entry).promise == promise {
                (*entry).signal = signal;
                (*entry).watch_next = (*signal).head;
                if !(*signal).head.is_null() {
                    (*(*signal).head).watch_previous = entry;
                }
                (*signal).head = entry;
                return;
            }
            entry = (*entry).next;
        }
    }
    unreachable!("timer must be registered before watching its signal")
}

/// Detach before recycling the scheduler entry or releasing its task.
pub(crate) unsafe fn detach(entry: *mut region::Subtask) {
    unsafe {
        let signal = (*entry).signal;
        if signal.is_null() {
            return;
        }
        let (previous, next) = ((*entry).watch_previous, (*entry).watch_next);
        if previous.is_null() {
            (*signal).head = next;
        } else {
            (*previous).watch_next = next;
        }
        if !next.is_null() {
            (*next).watch_previous = previous;
        }
        (*entry).signal = ptr::null_mut();
        (*entry).watch_previous = ptr::null_mut();
        (*entry).watch_next = ptr::null_mut();
    }
}

/// Abort pending timers, acknowledging host cancellation before detaching their roots.
///
/// # Safety
/// `signal` and both error-message strings are live. Watches refer to live scheduler entries.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_abort(
    signal: *mut u8,
    message: *mut u8,
    scope_error: *mut u8,
) {
    let signal = signal.cast::<Signal>();
    unsafe {
        if (*signal).aborted != 0 {
            return;
        }
        let caller = region::perry_task_current();
        let mut entry = (*signal).head;
        while !entry.is_null() {
            if (*entry).owner != caller {
                exception::perry_raise(exception::TYPE_ERROR, scope_error);
                return;
            }
            entry = (*entry).watch_next;
        }
        (*signal).aborted = 1;
        while !(*signal).head.is_null() {
            let entry = (*signal).head;
            let handle = (*entry).handle;
            host::waitable_join(handle, 0);
            let status = host::subtask_cancel(handle);
            region::perry_task_resume(caller);
            if let Some((promise, value)) = region::take_subtask(handle) {
                if status == host::SUBTASK_RETURNED {
                    promise::perry_promise_resolve(promise, value);
                } else {
                    perry_abort_reject(promise, message);
                }
                host::subtask_drop(handle);
            }
        }
        region::perry_task_resume(caller);
    }
}

#[cfg(all(test, feature = "collectible"))]
mod tests {
    use super::*;
    use crate::roots;

    #[test]
    fn completed_and_cancelled_timers_release_signal_edges() {
        region::perry_task_enter();
        roots::perry_gc_task_enter();
        roots::perry_gc_debug(3);
        let signal = perry_abort_new();
        let pin = roots::perry_gc_pin(signal);
        // SAFETY: the signal is pinned; pending timer payloads belong to the scheduler roots.
        unsafe {
            roots::perry_gc_boundary();
            let retained = roots::counters().live_bytes;
            for handle in 1..=2000 {
                let value = region::perry_alloc(1024);
                value.write(73);
                let promise = promise::perry_promise_new_typed(1);
                region::perry_subtask_register(handle, promise, value as usize as u64);
                perry_abort_watch(signal, handle, promise);
                roots::perry_gc_check();
                assert_eq!(value.read(), 73, "pending timer lost its payload");
                assert_eq!(region::perry_subtask_done(handle), 1);
                roots::perry_gc_check();
                assert_eq!(roots::counters().live_bytes, retained);
            }
            assert!((*signal.cast::<Signal>()).head.is_null());

            let caller = region::perry_task_current();
            region::perry_task_suspend();
            let other = region::perry_task_enter();
            roots::perry_gc_task_enter();
            let pending = promise::perry_promise_new_typed(0);
            region::perry_subtask_register(2001, pending, 7);
            perry_abort_watch(signal, 2001, pending);
            roots::perry_gc_check();
            region::perry_task_resume(caller);
            let scope_error = crate::string::from_static(
                "AbortSignal cancellation is limited to the current export task",
            );
            perry_abort(signal, scope_error, scope_error);
            assert_eq!(exception::perry_status(), exception::TYPE_ERROR);
            assert_eq!(perry_abort_aborted(signal), 0);
            region::current().exception = ptr::null_mut();
            region::perry_task_resume(other);
            assert_eq!(region::perry_task_cancel(), 1);
            roots::perry_gc_task_leave();
            region::perry_task_free();
            region::perry_task_resume(caller);
            roots::perry_gc_check();
            assert!((*signal.cast::<Signal>()).head.is_null());
            assert_eq!(roots::counters().live_bytes, retained);

            let promise = promise::perry_promise_new_typed(0);
            let settled = roots::perry_gc_pin(promise);
            region::perry_subtask_register(2001, promise, 17);
            perry_abort_watch(signal, 2001, promise);
            let message = crate::string::from_static("The operation was aborted");
            perry_abort(signal, message, message);
            perry_abort(signal, message, message);
            assert_eq!(promise.cast::<u32>().read(), promise::REJECTED);
            assert_eq!(perry_abort_aborted(signal), 1);
            assert!((*signal.cast::<Signal>()).head.is_null());
            assert_eq!(region::perry_subtask_done(2001), 0);
            roots::perry_gc_unpin(settled);
            roots::perry_gc_unpin(pin);
            roots::perry_gc_task_leave();
            region::perry_task_free();
            roots::perry_gc_boundary();
            roots::perry_gc_debug(0);
        }
    }
}
