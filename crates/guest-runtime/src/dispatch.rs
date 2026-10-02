//! Dispatcher for Perry runtime ABI function calls (`mem_call`, `mem_call_i32`).

use crate::http::{start_http_request, ResponseEntry};
use crate::io::{fail_with_error, print_stdout};
use crate::nanbox::{
    get_pointer_id, nanbox_pointer, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE,
    TAG_UNDEFINED,
};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub extern "C" fn string_new(offset: i32, len: i32) {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(offset as *const u8, len as usize) };
    if let Ok(s) = std::str::from_utf8(slice) {
        state.alloc_string(s);
    }
}

#[no_mangle]
pub extern "C" fn console_log(val: i64) {
    let state = get_state();
    let msg = state.get_string(val);
    print_stdout(&format!("{msg}\n"));
}

#[no_mangle]
pub extern "C" fn console_error(val: i64) {
    let state = get_state();
    let msg = state.get_string(val);
    crate::io::print_stderr(&format!("{msg}\n"));
}

fn dispatch_clocks(name: &str, raw_args: &[i64]) -> Option<i64> {
    match name {
        "date_now" => Some(crate::date::date_now()),
        "performance_now" => Some(crate::date::performance_now()),
        "date_new" | "date_new_val" => {
            let arg = raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::date::date_new_val(arg))
        }
        "date_get_time" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_time(arg))
        }
        "date_to_iso_string" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_to_iso_string(arg))
        }
        "date_get_full_year" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_full_year(arg))
        }
        "date_get_month" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_month(arg))
        }
        "date_get_date" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_date(arg))
        }
        "date_get_day" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_day(arg))
        }
        "date_get_hours" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_hours(arg))
        }
        "date_get_minutes" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_minutes(arg))
        }
        "date_get_seconds" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_seconds(arg))
        }
        "date_get_milliseconds" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::date::date_get_milliseconds(arg))
        }
        _ => None,
    }
}

fn dispatch_random(name: &str, raw_args: &[i64]) -> Option<i64> {
    match name {
        "math_random" => Some(crate::random::math_random()),
        "crypto_random_uuid" => Some(crate::random::crypto_random_uuid()),
        "$$cryptoFillRandom" => {
            let handle = raw_args.first().copied().unwrap_or(0);
            Some(crate::random::crypto_fill_random(handle))
        }
        "crypto_random_bytes" => {
            let arg = raw_args.first().copied().unwrap_or(0);
            Some(crate::random::crypto_random_bytes(arg))
        }
        _ => None,
    }
}

#[no_mangle]
pub extern "C" fn mem_call_clocks(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| String::from_utf16_lossy(s))
        .unwrap_or_default();

    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if let Some(res) = dispatch_clocks(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    crate::dispatch_pure::mem_call_pure(func_name_id, arg_count, base_addr)
}

#[no_mangle]
pub extern "C" fn mem_call_random(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| String::from_utf16_lossy(s))
        .unwrap_or_default();

    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if let Some(res) = dispatch_random(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    crate::dispatch_pure::mem_call_pure(func_name_id, arg_count, base_addr)
}

#[no_mangle]
pub extern "C" fn mem_call_clocks_random(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| String::from_utf16_lossy(s))
        .unwrap_or_default();

    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if let Some(res) = dispatch_clocks(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    if let Some(res) = dispatch_random(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    crate::dispatch_pure::mem_call_pure(func_name_id, arg_count, base_addr)
}

#[no_mangle]
pub extern "C" fn mem_call(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| String::from_utf16_lossy(s))
        .unwrap_or_default();

    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if let Some(res) = dispatch_clocks(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    if let Some(res) = dispatch_random(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    if name == "all" {
        let array = raw_args.get(1).or(raw_args.first()).copied().unwrap_or(0);
        if let Some(JsHandle::Array(items)) = state.get_handle(array) {
            let response_ids: Vec<_> = items
                .iter()
                .filter_map(|&item| match state.get_handle(item) {
                    Some(JsHandle::Response(id)) => Some(*id),
                    _ => None,
                })
                .collect();
            crate::http::wait_for_responses(crate::http::get_responses(), &response_ids)
                .unwrap_or_else(|error| fail_with_error(&error));
        }
    } else if name == "await_promise" {
        if let Some(JsHandle::Response(id)) =
            raw_args.first().and_then(|&arg| state.get_handle(arg))
        {
            crate::http::get_responses()[*id]
                .wait()
                .unwrap_or_else(|error| fail_with_error(&error));
        }
    }

    if matches!(
        name.as_str(),
        "fetch_url"
            | "fetch"
            | "fetch_with_options"
            | "fetch_request"
            | "response_json"
            | "json"
            | "response_text"
            | "text"
    ) {
        let result_i64 = dispatch_http(&name, &raw_args);
        unsafe {
            *(base_addr as *mut i64) = result_i64;
        }
        return 0.0;
    }

    if matches!(name.as_str(), "object_get" | "class_get_field") && raw_args.len() >= 2 {
        let target_handle = raw_args[0];
        if let Some(JsHandle::Response(id)) = state.get_handle(target_handle) {
            let id = *id;
            let key_str = state.get_string(raw_args[1]);
            let responses = crate::http::get_responses();
            let status = responses[id]
                .status()
                .unwrap_or_else(|error| fail_with_error(&error));
            let result_i64 = match key_str.as_str() {
                "status" => (status as f64).to_bits() as i64,
                "ok" => {
                    (if (200..300).contains(&status) {
                        TAG_TRUE
                    } else {
                        TAG_FALSE
                    }) as i64
                }
                _ => TAG_UNDEFINED as i64,
            };
            unsafe {
                *(base_addr as *mut i64) = result_i64;
            }
            return 0.0;
        }
    }

    crate::dispatch_pure::mem_call_pure(func_name_id, arg_count, base_addr)
}

fn dispatch_http(name: &str, raw_args: &[i64]) -> i64 {
    let state = get_state();
    if matches!(
        name,
        "fetch_url" | "fetch" | "fetch_with_options" | "fetch_request"
    ) {
        let args = if name == "fetch_request" {
            &raw_args[1..]
        } else {
            &raw_args[..]
        };
        if let Some(&url_arg) = args.first() {
            let url = state.get_string(url_arg);
            let options = if name == "fetch_with_options" {
                let mut options = serde_json::Map::new();
                for (index, key) in [(1, "method"), (2, "body"), (3, "headers"), (4, "redirect")] {
                    if let Some(&value) = args.get(index) {
                        if value as u64 != TAG_UNDEFINED {
                            options.insert(key.into(), state.to_js_value(value));
                        }
                    }
                }
                serde_json::Value::Object(options)
            } else {
                args.get(1)
                    .map(|&value| state.to_js_value(value))
                    .unwrap_or(serde_json::Value::Null)
            };
            let options = crate::http_options::parse_options(options)
                .unwrap_or_else(|error| fail_with_error(&error));
            match start_http_request(&url, options) {
                Ok(fut) => {
                    let responses = crate::http::get_responses();
                    let id = responses.len();
                    responses.push(ResponseEntry::InFlight {
                        url: url.clone(),
                        future_resp: fut,
                    });
                    let h_id = state.alloc_handle(JsHandle::Response(id));
                    return nanbox_pointer(h_id);
                }
                Err(e) => {
                    fail_with_error(&format!("HTTP fetch initialization error for {url}: {e}"));
                }
            }
        }
    } else if name == "response_json" || name == "json" {
        let handle = raw_args.first().copied().unwrap_or(0);
        let resp_id = match state.get_handle(handle) {
            Some(JsHandle::Response(id)) => Some(*id),
            _ => get_pointer_id(handle),
        };
        if let Some(id) = resp_id {
            match crate::http::get_response_body(id) {
                Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(parsed) => return state.from_js_value(parsed),
                    Err(e) => {
                        fail_with_error(&format!("JSON parse error: {e}"));
                    }
                },
                Err(e) => {
                    fail_with_error(&e);
                }
            }
        } else {
            fail_with_error("Invalid response handle passed to .json()");
        }
    } else if name == "response_text" || name == "text" {
        let handle = raw_args.first().copied().unwrap_or(0);
        let resp_id = match state.get_handle(handle) {
            Some(JsHandle::Response(id)) => Some(*id),
            _ => get_pointer_id(handle),
        };
        if let Some(id) = resp_id {
            match crate::http::get_response_body(id) {
                Ok(body) => {
                    return state.alloc_string(&body);
                }
                Err(e) => {
                    fail_with_error(&e);
                }
            }
        } else {
            fail_with_error("Invalid response handle passed to .text()");
        }
    }
    0
}

#[no_mangle]
pub extern "C" fn mem_call_i32(func_name_id: f64, arg_count: f64, base_addr: i32) -> i32 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| String::from_utf16_lossy(s))
        .unwrap_or_default();
    let count = arg_count as usize;

    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if name == "has_exception" {
        return crate::stubs::has_exception();
    } else if matches!(name.as_str(), "string_eq" | "js_strict_eq" | "js_loose_eq") {
        if let [left, right] = raw_args.as_slice() {
            return i32::from(crate::equality::equal(
                state,
                *left,
                *right,
                name == "js_loose_eq",
            ));
        }
    } else if name == "is_truthy" {
        if let Some(&arg) = raw_args.first() {
            let bits = arg as u64;
            if bits == TAG_UNDEFINED || bits == TAG_NULL || bits == TAG_FALSE {
                return 0;
            }
            if bits == TAG_TRUE {
                return 1;
            }
            if bits >> 48 == STRING_TAG {
                return i32::from(!state.get_string(arg).is_empty());
            }
            if bits >> 48 == POINTER_TAG {
                return 1;
            }
            let f = f64::from_bits(bits);
            if f == 0.0 || f.is_nan() {
                return 0;
            }
            return 1;
        }
    }

    0
}
