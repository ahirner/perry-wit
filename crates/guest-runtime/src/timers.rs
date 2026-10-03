//! One-shot timers own their callbacks and clock subscriptions until delivery or cancellation.

use crate::bindings::wasi::clocks::monotonic_clock;
use crate::bindings::wasi::io::poll::Pollable;
use crate::callbacks::guest_callback_invoke;
use crate::nanbox::{nanbox_pointer, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

const MAX_TIMER_ID: u64 = (1 << 53) - 1;

/// Numeric IDs never repeat, including after pending entries have been removed.
#[derive(Default)]
pub(crate) struct Timers {
    next_id: u64,
    pending: Vec<PendingTimer>,
}

/// The pollable drops at an ordinary invocation boundary, never during post-return.
struct PendingTimer {
    id: u64,
    deadline: u64,
    callback: i64,
    arguments: i64,
    pollable: Pollable,
}

impl Timers {
    /// Pending work owns callback graphs even when no global references remain.
    pub(crate) fn roots(&self) -> impl Iterator<Item = i64> + '_ {
        self.pending
            .iter()
            .flat_map(|timer| [timer.callback, timer.arguments])
    }

    /// Releases subscriptions at an ordinary invocation boundary after a guest error.
    pub(crate) fn cancel_all(&mut self) {
        self.pending.clear();
    }

    fn take_next(&mut self) -> Option<PendingTimer> {
        let index = self
            .pending
            .iter()
            .enumerate()
            .min_by_key(|(_, timer)| (timer.deadline, timer.id))
            .map(|(index, _)| index)?;
        Some(self.pending.swap_remove(index))
    }
}

/// Only statically identified timer calls use this capability-specific bridge.
#[no_mangle]
pub(crate) extern "C" fn mem_call_timers(name: f64, count: f64, base: i32) -> f64 {
    crate::dispatch::mem_call_sync(name, count, base, dispatch_call)
}

fn dispatch_call(name: &str, args: &[i64]) -> Option<i64> {
    let args = if args.first() == Some(&(TAG_UNDEFINED as i64)) {
        &args[1..]
    } else {
        args
    };
    let argument = |index| args.get(index).copied().unwrap_or(TAG_UNDEFINED as i64);
    Some(match name {
        "timer_schedule" => schedule(argument(0), argument(1), argument(2)),
        "timer_cancel" => {
            cancel(argument(0));
            TAG_UNDEFINED as i64
        }
        _ => return None,
    })
}

/// Validates before allocating a subscription and retains raw callback argument values.
fn schedule(callback: i64, delay: i64, arguments: i64) -> i64 {
    let state = get_state();
    if !matches!(state.get_handle(callback), Some(JsHandle::Closure(_))) {
        state.current_exception = Some("TypeError: setTimeout requires a guest function".into());
        return TAG_UNDEFINED as i64;
    }
    if !matches!(state.get_handle(arguments), Some(JsHandle::Array(_))) {
        state.current_exception = Some("TypeError: Invalid timer arguments".into());
        return TAG_UNDEFINED as i64;
    }
    if state.timers.next_id == MAX_TIMER_ID || state.timers.pending.try_reserve(1).is_err() {
        state.current_exception = Some("RangeError: Timer capacity exhausted".into());
        return TAG_UNDEFINED as i64;
    }
    let delay = state.to_number(delay);
    let milliseconds = if delay.is_finite() && (1.0..=i32::MAX as f64).contains(&delay) {
        delay.trunc() as u64
    } else {
        1
    };
    let deadline = monotonic_clock::now().saturating_add(milliseconds * 1_000_000);
    let pollable = monotonic_clock::subscribe_instant(deadline);
    state.timers.next_id += 1;
    let id = state.timers.next_id;
    state.timers.pending.push(PendingTimer {
        id,
        deadline,
        callback,
        arguments,
        pollable,
    });
    (id as f64).to_bits() as i64
}

fn cancel(value: i64) {
    let state = get_state();
    let number = state.to_number(value);
    if number.is_finite() && (1.0..=MAX_TIMER_ID as f64).contains(&number) && number.fract() == 0.0
    {
        if let Some(index) = state
            .timers
            .pending
            .iter()
            .position(|timer| timer.id == number as u64)
        {
            state.timers.pending.swap_remove(index);
        }
    }
}

/// Drives timers in deadline/registration order before the host invocation returns.
/// An uncaught callback error cancels all remaining work and reaches the normal ABI error path.
#[no_mangle]
pub(crate) extern "C" fn timers_drain() {
    while get_state().current_exception.is_none() {
        let Some(timer) = get_state().timers.take_next() else {
            return;
        };
        timer.pollable.block();
        drop(timer.pollable);
        guest_callback_invoke(timer.callback, timer.arguments);
    }
    get_state().timers.cancel_all();
}

#[no_mangle]
pub(crate) extern "C" fn set_timeout(callback: i64, delay: i64) -> i64 {
    let arguments = nanbox_pointer(get_state().alloc_handle(JsHandle::Array(Vec::new())));
    schedule(callback, delay, arguments)
}

#[no_mangle]
pub(crate) extern "C" fn clear_timeout(value: i64) {
    cancel(value);
}

#[no_mangle]
pub(crate) extern "C" fn clear_interval(value: i64) {
    cancel(value);
}
