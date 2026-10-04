//! JavaScript Date runtime support for WASI Preview 2.

use crate::clocks::wall_clock_now_ms;
use crate::nanbox::{nanbox_pointer, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle, RuntimeState};

/// Format an f64 timestamp as an ISO 8601 string `YYYY-MM-DDTHH:mm:ss.sssZ`.
#[must_use]
pub(crate) fn format_iso(ts: f64) -> Option<String> {
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return None;
    }
    let ts_ms = ts as i64;
    let secs = ts_ms.div_euclid(1000);
    let millis = ts_ms.rem_euclid(1000) as u32;

    let (year, month, day) = datealgo::rd_to_date(secs.div_euclid(86400) as i32);
    let daytime = secs.rem_euclid(86400);
    let (hour, minute, second) = (daytime / 3600, daytime / 60 % 60, daytime % 60);
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{year:+07}")
    };
    Some(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    ))
}

fn get_date_timestamp(state: &RuntimeState, val: i64) -> f64 {
    if let Some(JsHandle::Date(ts)) = state.get_handle(val) {
        *ts
    } else {
        f64::from_bits(val as u64)
    }
}

#[no_mangle]
pub extern "C" fn date_now() -> i64 {
    let now = wall_clock_now_ms();
    now.to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_new_val(arg: i64) -> i64 {
    let state = get_state();
    let bits = arg as u64;
    let ts = if bits == TAG_UNDEFINED {
        f64::NAN
    } else if bits == TAG_NULL || bits == TAG_FALSE {
        0.0
    } else if bits == TAG_TRUE {
        1.0
    } else if let Some(JsHandle::Date(existing)) = state.get_handle(arg) {
        *existing
    } else {
        let f = f64::from_bits(bits);
        if f.is_finite() && f.abs() <= 8.64e15 {
            f.trunc()
        } else {
            f64::NAN
        }
    };

    let id = state.alloc_handle(JsHandle::Date(ts));
    nanbox_pointer(id)
}

#[no_mangle]
pub extern "C" fn date_get_time(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    ts.to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_to_iso_string(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if let Some(iso) = format_iso(ts) {
        state.alloc_string(&iso)
    } else {
        state.current_exception = Some("RangeError: Invalid time value".to_string());
        TAG_UNDEFINED as i64
    }
}

#[no_mangle]
pub extern "C" fn performance_now() -> i64 {
    let now = crate::clocks::monotonic_clock_now_ms();
    now.to_bits() as i64
}
