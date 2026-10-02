//! Dispatcher for Perry runtime ABI function calls (`mem_call`, `mem_call_i32`).

use crate::http::{ResponseEntry, start_http_request};
use crate::io::{fail_with_error, print_stdout};
use crate::nanbox::{
    POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED, get_pointer_id,
    nanbox_pointer, nanbox_string,
};
use crate::state::{JsHandle, get_state};

#[no_mangle]
pub extern "C" fn string_new(offset: i32, len: i32) {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(offset as *const u8, len as usize) };
    if let Ok(s) = std::str::from_utf8(slice) {
        state.strings.push(s.to_string());
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

#[no_mangle]
pub extern "C" fn mem_call(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let count = arg_count as usize;
    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    let mut result_i64: i64 = 0;

    if matches!(
        name.as_str(),
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
                    let id = state.responses.len();
                    state.responses.push(ResponseEntry::InFlight {
                        url,
                        future_resp: fut,
                    });
                    let h_id = state.alloc_handle(JsHandle::Response(id));
                    result_i64 = nanbox_pointer(h_id);
                }
                Err(e) => {
                    fail_with_error(&format!("HTTP fetch initialization error for {url}: {e}"));
                }
            }
        }
    } else if name == "all" {
        // Promise.all(iterable)
        let arr_arg = if raw_args.len() >= 2 {
            raw_args[1]
        } else {
            raw_args.first().copied().unwrap_or(0)
        };
        if let Some(JsHandle::Array(items)) = state.get_handle(arr_arg).cloned() {
            for &item in &items {
                if let Some(JsHandle::Response(resp_id)) = state.get_handle(item) {
                    let resp_id = *resp_id;
                    if let Err(e) = state.get_response_body(resp_id) {
                        fail_with_error(&e);
                    }
                }
            }
            let res_arr_id = state.alloc_handle(JsHandle::Array(items));
            result_i64 = nanbox_pointer(res_arr_id);
        }
    } else if name == "response_json" || name == "json" {
        let handle = raw_args.first().copied().unwrap_or(0);
        let resp_id = match state.get_handle(handle) {
            Some(JsHandle::Response(id)) => Some(*id),
            _ => get_pointer_id(handle),
        };
        if let Some(id) = resp_id {
            match state.get_response_body(id) {
                Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
                    Ok(parsed) => result_i64 = state.from_js_value(parsed),
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
            match state.get_response_body(id) {
                Ok(body) => {
                    let str_id = state.strings.len();
                    state.strings.push(body);
                    result_i64 = nanbox_string(str_id);
                }
                Err(e) => {
                    fail_with_error(&e);
                }
            }
        } else {
            fail_with_error("Invalid response handle passed to .text()");
        }
    } else if name == "array_new" {
        let h_id = state.alloc_handle(JsHandle::Array(Vec::new()));
        result_i64 = nanbox_pointer(h_id);
    } else if name == "array_push" {
        if raw_args.len() >= 2 {
            let arr_handle = raw_args[0];
            let item = raw_args[1];
            if let Some(JsHandle::Array(arr)) = state.get_handle_mut(arr_handle) {
                arr.push(item);
            }
            result_i64 = arr_handle;
        }
    } else if name == "array_get" || name == "object_get_dynamic" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let idx_val = raw_args[1];
            let bits = idx_val as u64;
            let idx = if (bits >> 48) < 0x7ff8 {
                f64::from_bits(bits) as usize
            } else {
                (bits & 0xFFFF_FFFF) as usize
            };
            if let Some(h) = state.get_handle(target_handle).cloned() {
                match h {
                    JsHandle::Array(arr) => {
                        if let Some(&elem) = arr.get(idx) {
                            result_i64 = elem;
                        }
                    }
                    JsHandle::Json(serde_json::Value::Array(arr)) => {
                        if let Some(elem) = arr.get(idx) {
                            result_i64 = state.from_js_value(elem.clone());
                        }
                    }
                    JsHandle::Json(serde_json::Value::Object(map)) => {
                        let key_str = state.get_string(idx_val);
                        if let Some(v) = map.get(&key_str) {
                            result_i64 = state.from_js_value(v.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
    } else if name == "string_len" || name == "array_length" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(h) = state.get_handle(arg) {
            match h {
                JsHandle::Array(arr) => {
                    result_i64 = (arr.len() as f64).to_bits() as i64;
                }
                JsHandle::Json(serde_json::Value::Array(arr)) => {
                    result_i64 = (arr.len() as f64).to_bits() as i64;
                }
                _ => {
                    let len = state.get_string(arg).len();
                    result_i64 = (len as f64).to_bits() as i64;
                }
            }
        } else {
            let len = state.get_string(arg).len();
            result_i64 = (len as f64).to_bits() as i64;
        }
    } else if name == "object_new" {
        let h_id = state.alloc_handle(JsHandle::Json(serde_json::Value::Object(
            serde_json::Map::new(),
        )));
        result_i64 = nanbox_pointer(h_id);
    } else if name == "object_set" {
        if raw_args.len() >= 3 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            let val_json = state.to_js_value(raw_args[2]);
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle_mut(target_handle)
            {
                map.insert(key_str, val_json);
            }
            result_i64 = target_handle;
        }
    } else if name == "object_assign" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let source_handle = raw_args[1];
            let source_json = state.to_js_value(source_handle);
            if let Some(JsHandle::Json(serde_json::Value::Object(target_map))) =
                state.get_handle_mut(target_handle)
            {
                if let serde_json::Value::Object(src_map) = source_json {
                    for (k, v) in src_map {
                        target_map.insert(k, v);
                    }
                }
            }
            result_i64 = target_handle;
        }
    } else if name == "object_get" || name == "class_get_field" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            if let Some(JsHandle::Response(id)) = state.get_handle(target_handle) {
                let id = *id;
                let status = state.responses[id]
                    .status()
                    .unwrap_or_else(|error| fail_with_error(&error));
                result_i64 = match key_str.as_str() {
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
            }
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle(target_handle)
            {
                if let Some(v) = map.get(&key_str) {
                    let v_clone = v.clone();
                    result_i64 = state.from_js_value(v_clone);
                }
            }
        }
    } else if name == "json_stringify" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let val_json = state.to_js_value(arg);
        let json_str = serde_json::to_string_pretty(&val_json).unwrap_or_else(|_| "{}".to_string());
        let str_id = state.strings.len();
        state.strings.push(json_str);
        result_i64 = nanbox_string(str_id);
    } else if name == "json_parse" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let s = state.get_string(arg);
        let val_json: serde_json::Value = serde_json::from_str(&s)
            .unwrap_or_else(|error| fail_with_error(&format!("JSON parse error: {error}")));
        result_i64 = state.from_js_value(val_json);
    } else if name == "console_log" || name == "log" {
        let arg = raw_args.last().copied().unwrap_or(0);
        let msg = state.get_string(arg);
        print_stdout(&format!("{msg}\n"));
    } else if name == "console_error" || name == "error" {
        let arg = raw_args.last().copied().unwrap_or(0);
        let msg = state.get_string(arg);
        crate::io::print_stderr(&format!("{msg}\n"));
    } else if name == "string_concat" || name == "js_add" {
        if raw_args.len() >= 2 {
            let s_a = state.get_string(raw_args[0]);
            let s_b = state.get_string(raw_args[1]);
            let res = format!("{s_a}{s_b}");
            let str_id = state.strings.len();
            state.strings.push(res);
            result_i64 = nanbox_string(str_id);
        }
    } else if name == "process_exit" || name == "exit" {
        let code = raw_args.last().copied().unwrap_or(0);
        crate::io::exit_process(f64::from_bits(code as u64) as i32);
    } else if name == "await_promise" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Response(resp_id)) = state.get_handle(arg) {
            let resp_id = *resp_id;
            if let Err(e) = state.get_response_body(resp_id) {
                fail_with_error(&e);
            }
        }
        result_i64 = arg;
    }

    // Write result back to base_addr
    unsafe {
        *(base_addr as *mut i64) = result_i64;
    }

    0.0
}

#[no_mangle]
pub extern "C" fn mem_call_i32(func_name_id: f64, arg_count: f64, base_addr: i32) -> i32 {
    let state = get_state();
    let name_idx = func_name_id as usize;
    let name = state
        .strings
        .get(name_idx)
        .map(|s| s.as_str())
        .unwrap_or("");
    let count = arg_count as usize;

    let mut raw_args = Vec::with_capacity(count);
    let ptr = base_addr as *const i64;
    for i in 0..count {
        raw_args.push(unsafe { *ptr.add(i) });
    }

    if matches!(name, "string_eq" | "js_strict_eq" | "js_loose_eq") {
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
