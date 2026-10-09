//! Instance-local scalar process status. Immediate exit is a codegen boundary operation:
//! it cancels native work and releases the current task before invoking WASI exit.
use core::cell::UnsafeCell;

use crate::UNDEFINED_NUMBER;
struct Process(UnsafeCell<Option<f64>>);
// SAFETY: a component instance runs one thread; no borrow survives an import.
unsafe impl Sync for Process {}
static PROCESS: Process = Process(UnsafeCell::new(None));

#[unsafe(no_mangle)]
pub extern "C" fn perry_process_get_exit_code() -> f64 {
    // SAFETY: one thread accesses instance state.
    unsafe { (*PROCESS.0.get()).unwrap_or(f64::from_bits(UNDEFINED_NUMBER)) }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_process_clear_exit_code() {
    // SAFETY: one thread accesses instance state.
    unsafe { *PROCESS.0.get() = None };
}

/// Store a numeric exit code, or return a catchable RangeError status without changing it.
#[unsafe(no_mangle)]
pub extern "C" fn perry_process_set_exit_code(value: f64) -> i32 {
    if !value.is_finite()
        || libm::trunc(value) != value
        || libm::fabs(value) > 9_007_199_254_740_991.0
    {
        return crate::exception::raise_error(
            crate::exception::RANGE_ERROR,
            "exit code must be a safe integer",
        );
    }
    // SAFETY: one thread accesses instance state.
    unsafe { *PROCESS.0.get() = Some(value) };
    0
}

/// The explicit status modulo 256, or -1 when no status was assigned.
#[unsafe(no_mangle)]
pub extern "C" fn perry_process_exit_status() -> i32 {
    // SAFETY: one thread accesses instance state; stored values are safe integers.
    unsafe { (*PROCESS.0.get()).map_or(-1, |value| ((value as i64) & 255) as i32) }
}
