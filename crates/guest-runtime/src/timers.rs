//! Timers own their callbacks and clock subscriptions until delivery or cancellation.

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
    callback: i64,
    arguments: i64,
    mode: TimerMode,
    delay_ns: u64,
    status: TimerStatus,
}

/// Controls whether delivery removes the entry or rearms its original delay.
#[derive(Clone, Copy)]
enum TimerMode {
    Timeout,
    Interval,
}

/// A firing interval retains its identity and captures, but owns no subscription.
enum TimerStatus {
    Waiting { deadline: u64, pollable: Pollable },
    Running,
}

/// Moves the subscription out of the queue while retaining the delivery's raw values.
struct TimerDelivery {
    id: u64,
    callback: i64,
    arguments: i64,
    mode: TimerMode,
    delay_ns: u64,
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

    /// Removes one-shots and leaves firing intervals discoverable by cancellation.
    fn take_next(&mut self) -> Option<TimerDelivery> {
        let index = self
            .pending
            .iter()
            .enumerate()
            .filter_map(|(index, timer)| match timer.status {
                TimerStatus::Waiting { deadline, .. } => Some(((deadline, timer.id), index)),
                TimerStatus::Running => None,
            })
            .min_by_key(|(order, _)| *order)
            .map(|(_, index)| index)?;
        let timer = &mut self.pending[index];
        let TimerStatus::Waiting { pollable, .. } =
            std::mem::replace(&mut timer.status, TimerStatus::Running)
        else {
            return None;
        };
        let delivery = TimerDelivery {
            id: timer.id,
            callback: timer.callback,
            arguments: timer.arguments,
            mode: timer.mode,
            delay_ns: timer.delay_ns,
            pollable,
        };
        if matches!(delivery.mode, TimerMode::Timeout) {
            self.pending.swap_remove(index);
        }
        Some(delivery)
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
        "timer_schedule" => schedule(argument(0), argument(1), argument(2), TimerMode::Timeout),
        "timer_interval" => schedule(argument(0), argument(1), argument(2), TimerMode::Interval),
        "timer_cancel" => {
            cancel(argument(0));
            TAG_UNDEFINED as i64
        }
        _ => return None,
    })
}

/// Validates before allocating a subscription and retains raw callback argument values.
fn schedule(callback: i64, delay: i64, arguments: i64, mode: TimerMode) -> i64 {
    let state = get_state();
    if !matches!(state.get_handle(callback), Some(JsHandle::Closure(_))) {
        let name = match mode {
            TimerMode::Timeout => "setTimeout",
            TimerMode::Interval => "setInterval",
        };
        state.current_exception = Some(format!("TypeError: {name} requires a guest function"));
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
    let delay_ns = milliseconds * 1_000_000;
    let deadline = monotonic_clock::now().saturating_add(delay_ns);
    let pollable = monotonic_clock::subscribe_instant(deadline);
    state.timers.next_id += 1;
    let id = state.timers.next_id;
    state.timers.pending.push(PendingTimer {
        id,
        callback,
        arguments,
        mode,
        delay_ns,
        status: TimerStatus::Waiting { deadline, pollable },
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

/// Delivers one timer; the ABI driver reclaims temporaries between successful callbacks.
/// A running interval remains cancellable and is rearmed from this callback's start time.
#[no_mangle]
pub(crate) extern "C" fn timers_step() -> i32 {
    if get_state().current_exception.is_some() {
        get_state().timers.cancel_all();
        return 0;
    }
    let Some(timer) = get_state().timers.take_next() else {
        return 0;
    };
    timer.pollable.block();
    drop(timer.pollable);
    let started = if matches!(timer.mode, TimerMode::Interval) {
        monotonic_clock::now()
    } else {
        0
    };
    guest_callback_invoke(timer.callback, timer.arguments);
    if get_state().current_exception.is_some() {
        get_state().timers.cancel_all();
        return 0;
    }
    if matches!(timer.mode, TimerMode::Interval) {
        if let Some(pending) = get_state()
            .timers
            .pending
            .iter_mut()
            .find(|pending| pending.id == timer.id)
        {
            let deadline = started.saturating_add(timer.delay_ns);
            pending.status = TimerStatus::Waiting {
                deadline,
                pollable: monotonic_clock::subscribe_instant(deadline),
            };
        }
    }
    1
}

#[no_mangle]
pub(crate) extern "C" fn set_timeout(callback: i64, delay: i64) -> i64 {
    let arguments = nanbox_pointer(get_state().alloc_handle(JsHandle::Array(Vec::new())));
    schedule(callback, delay, arguments, TimerMode::Timeout)
}

#[no_mangle]
pub(crate) extern "C" fn set_interval(callback: i64, delay: i64) -> i64 {
    let arguments = nanbox_pointer(get_state().alloc_handle(JsHandle::Array(Vec::new())));
    schedule(callback, delay, arguments, TimerMode::Interval)
}

#[no_mangle]
pub(crate) extern "C" fn clear_timeout(value: i64) {
    cancel(value);
}

#[no_mangle]
pub(crate) extern "C" fn clear_interval(value: i64) {
    cancel(value);
}
