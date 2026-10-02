//! Clocks implementation using WASI Preview 2 wasi:clocks interfaces.

use crate::bindings::wasi::clocks::{monotonic_clock, wall_clock};

/// Returns epoch milliseconds as an f64.
#[must_use]
pub(crate) fn wall_clock_now_ms() -> f64 {
    let dt = wall_clock::now();
    (dt.seconds as f64 * 1000.0) + (f64::from(dt.nanoseconds) / 1_000_000.0)
}

/// Returns elapsed monotonic time in milliseconds as an f64.
#[must_use]
pub(crate) fn monotonic_clock_now_ms() -> f64 {
    let nanos = monotonic_clock::now();
    (nanos as f64) / 1_000_000.0
}
