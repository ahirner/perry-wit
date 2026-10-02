//! Pure runtime dispatch (`mem_call_pure`, `mem_call_i32`).
//!
//! Contains zero capability dependencies: no WASI HTTP, no WASI clocks,
//! only pure memory operations, JSON manipulation, arrays, objects, strings,
//! and standard console/exit I/O.

use crate::io::{fail_with_error, print_stdout};
use crate::nanbox::{
    nanbox_pointer, nanbox_string, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL,
    TAG_TRUE, TAG_UNDEFINED,
};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub extern "C" fn mem_call_pure(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
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

    if name == "array_new" {
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
        let json_str = serde_json::to_string(&val_json).unwrap_or_else(|_| "{}".to_string());
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
            let args = [raw_args[0], raw_args[1]];
            if name == "js_add"
                && args
                    .iter()
                    .all(|&value| !matches!((value as u64) >> 48, STRING_TAG | POINTER_TAG))
            {
                let [left, right] = args.map(|value| match value as u64 {
                    TAG_TRUE => 1.0,
                    TAG_FALSE | TAG_NULL => 0.0,
                    TAG_UNDEFINED => f64::NAN,
                    bits => f64::from_bits(bits),
                });
                result_i64 = (left + right).to_bits() as i64;
            } else {
                let mut text = state.get_string(args[0]);
                text.push_str(&state.get_string(args[1]));
                let str_id = state.strings.len();
                state.strings.push(text);
                result_i64 = nanbox_string(str_id);
            }
        }
    } else if name == "process_exit" || name == "exit" {
        let code = raw_args.last().copied().unwrap_or(0);
        crate::io::exit_process(f64::from_bits(code as u64) as i32);
    }

    unsafe {
        *(base_addr as *mut i64) = result_i64;
    }

    0.0
}
