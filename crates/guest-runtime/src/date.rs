//! JavaScript Date runtime support for WASI Preview 2.

use crate::clocks::wall_clock_now_ms;
use crate::nanbox::{nanbox_pointer, nanbox_string, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle, RuntimeState};

/// Convert Unix timestamp (seconds) to UTC date components.
/// Returns (year, month [1-12], day [1-31], hour [0-23], minute [0-59], second [0-59], day_of_week [0-6]).
#[must_use]
pub(crate) fn timestamp_to_components(secs: i64) -> (i32, u32, u32, u32, u32, u32, u32) {
    let is_negative = secs < 0;
    let abs_secs = if is_negative { -secs } else { secs } as u64;

    let second = (abs_secs % 60) as u32;
    let minute = ((abs_secs / 60) % 60) as u32;
    let hour = ((abs_secs / 3600) % 24) as u32;

    let days = if is_negative {
        -((abs_secs / 86400) as i64) - if (abs_secs % 86400) != 0 { 1 } else { 0 }
    } else {
        (abs_secs / 86400) as i64
    };

    let (hour, minute, second) = if is_negative && (abs_secs % 86400) != 0 {
        let remaining = abs_secs % 86400;
        let adjusted = 86400 - remaining;
        (
            ((adjusted / 3600) % 24) as u32,
            ((adjusted / 60) % 60) as u32,
            (adjusted % 60) as u32,
        )
    } else {
        (hour, minute, second)
    };

    // Days since 1970-01-01 (1970-01-01 was Thursday, day 4)
    let day_of_week = ((days + 4).rem_euclid(7)) as u32;

    // Howard Hinnant's algorithm
    let z = days + 719468;
    let era = if z >= 0 {
        z / 146097
    } else {
        (z - 146096) / 146097
    };
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, hour, minute, second, day_of_week)
}

/// Format year for ISO 8601 string.
fn iso_year(year: i32) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else if year < 0 {
        format!("-{:06}", -year)
    } else {
        format!("+{year:06}")
    }
}

/// Format an f64 timestamp as an ISO 8601 string `YYYY-MM-DDTHH:mm:ss.sssZ`.
#[must_use]
pub(crate) fn format_iso(ts: f64) -> Option<String> {
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return None;
    }
    let ts_ms = ts as i64;
    let secs = ts_ms.div_euclid(1000);
    let millis = ts_ms.rem_euclid(1000) as u32;

    let (year, month, day, hour, minute, second, _) = timestamp_to_components(secs);
    Some(format!(
        "{}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        iso_year(year),
        month,
        day,
        hour,
        minute,
        second,
        millis
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
        let str_id = state.strings.len();
        state.strings.push(iso);
        nanbox_string(str_id)
    } else {
        state.current_exception = Some("RangeError: Invalid time value".to_string());
        let str_id = state.strings.len();
        state.strings.push("Invalid Date".to_string());
        nanbox_string(str_id)
    }
}

#[no_mangle]
pub extern "C" fn date_get_full_year(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (year, _, _, _, _, _, _) = timestamp_to_components(secs);
    (year as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_month(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, month, _, _, _, _, _) = timestamp_to_components(secs);
    ((month - 1) as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_date(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, _, day, _, _, _, _) = timestamp_to_components(secs);
    (day as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_day(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, _, _, _, _, _, dow) = timestamp_to_components(secs);
    (dow as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_hours(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, _, _, hour, _, _, _) = timestamp_to_components(secs);
    (hour as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_minutes(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, _, _, _, minute, _, _) = timestamp_to_components(secs);
    (minute as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_seconds(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let secs = (ts as i64).div_euclid(1000);
    let (_, _, _, _, _, second, _) = timestamp_to_components(secs);
    (second as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn date_get_milliseconds(arg: i64) -> i64 {
    let state = get_state();
    let ts = get_date_timestamp(state, arg);
    if !ts.is_finite() || ts.abs() > 8.64e15 {
        return f64::NAN.to_bits() as i64;
    }
    let ms = (ts as i64).rem_euclid(1000);
    (ms as f64).to_bits() as i64
}

#[no_mangle]
pub extern "C" fn performance_now() -> i64 {
    let now = crate::clocks::monotonic_clock_now_ms();
    now.to_bits() as i64
}
