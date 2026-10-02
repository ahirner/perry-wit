//! Pure runtime dispatch (`mem_call_pure`, `mem_call_i32`).
//!
//! Contains zero capability dependencies: no WASI HTTP, no WASI clocks,
//! only pure memory operations, JSON manipulation, arrays, objects, strings,
//! and standard console/exit I/O.

use crate::io::{fail_with_error, print_stdout};
use crate::nanbox::{
    nanbox_pointer, nanbox_string, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE,
    TAG_UNDEFINED,
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

    if name == "get_exception" {
        result_i64 = crate::stubs::get_exception();
    } else if name == "throw_value" {
        crate::stubs::throw_value(raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64));
        result_i64 = TAG_UNDEFINED as i64;
    } else if name == "await_promise" {
        result_i64 = raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
    } else if name == "all" {
        let array = raw_args.get(1).or(raw_args.first()).copied().unwrap_or(0);
        if let Some(JsHandle::Array(items)) = state.get_handle(array).cloned() {
            let id = state.alloc_handle(JsHandle::Array(items));
            result_i64 = nanbox_pointer(id);
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
    } else if name == "uint8array_new" || name == "buffer_alloc" {
        result_i64 =
            crate::stubs::uint8array_new(raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64));
    } else if name == "uint8array_from" {
        result_i64 = crate::stubs::uint8array_from(
            raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64),
        );
    } else if name == "uint8array_length" || name == "buffer_length" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Uint8Array(v)) = state.get_handle(arg) {
            result_i64 = (v.byte_length as f64).to_bits() as i64;
        }
    } else if name == "uint8array_get" || name == "buffer_get" {
        if raw_args.len() >= 2 {
            let handle = raw_args[0];
            let idx_val = raw_args[1];
            let bits = idx_val as u64;
            let idx = if (bits >> 48) < 0x7ff8 {
                f64::from_bits(bits) as usize
            } else {
                (bits & 0xFFFF_FFFF) as usize
            };
            if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle) {
                if let Some(b) = v.get(idx) {
                    result_i64 = (b as f64).to_bits() as i64;
                } else {
                    result_i64 = TAG_UNDEFINED as i64;
                }
            }
        }
    } else if name == "uint8array_set" || name == "buffer_set" {
        if raw_args.len() >= 3 {
            let handle = raw_args[0];
            let idx_val = raw_args[1];
            let val_val = raw_args[2];
            let idx_bits = idx_val as u64;
            let idx = if (idx_bits >> 48) < 0x7ff8 {
                f64::from_bits(idx_bits) as usize
            } else {
                (idx_bits & 0xFFFF_FFFF) as usize
            };
            let val_byte = state.to_uint8(val_val);
            if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle) {
                v.set(idx, val_byte);
            }
        }
    } else if name == "buffer_slice" {
        if !raw_args.is_empty() {
            let handle = raw_args[0];
            let start = if raw_args.len() >= 2 {
                let bits = raw_args[1] as u64;
                if (bits >> 48) < 0x7ff8 {
                    let f = f64::from_bits(bits);
                    if f.is_finite() && f > 0.0 {
                        f as usize
                    } else {
                        0
                    }
                } else {
                    (bits & 0xFFFF_FFFF) as usize
                }
            } else {
                0
            };
            let end = if raw_args.len() >= 3 && raw_args[2] as u64 != TAG_UNDEFINED {
                let bits = raw_args[2] as u64;
                if (bits >> 48) < 0x7ff8 {
                    let f = f64::from_bits(bits);
                    if f.is_finite() && f >= 0.0 {
                        Some(f as usize)
                    } else {
                        Some(0)
                    }
                } else {
                    Some((bits & 0xFFFF_FFFF) as usize)
                }
            } else {
                None
            };
            if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle).cloned() {
                let subview = v.subview(start, end);
                let id = state.alloc_handle(JsHandle::Uint8Array(subview));
                result_i64 = nanbox_pointer(id);
            } else {
                let id =
                    state.alloc_handle(JsHandle::Uint8Array(crate::buffer::Uint8ArrayView::new(0)));
                result_i64 = nanbox_pointer(id);
            }
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
                    JsHandle::Uint8Array(v) => {
                        if let Some(b) = v.get(idx) {
                            result_i64 = (b as f64).to_bits() as i64;
                        } else {
                            result_i64 = TAG_UNDEFINED as i64;
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
            } else if (target_handle as u64) >> 48 == STRING_TAG {
                let s = state.get_string(target_handle);
                if let Some(c) = s.chars().nth(idx) {
                    let str_id = state.strings.len();
                    state.strings.push(c.to_string());
                    result_i64 = nanbox_string(str_id);
                } else {
                    result_i64 = TAG_UNDEFINED as i64;
                }
            }
        }
    } else if name == "string_charAt" || name == "string_char_at" {
        if raw_args.len() >= 2 {
            let s = state.get_string(raw_args[0]);
            let idx_val = raw_args[1];
            let idx_f = f64::from_bits(idx_val as u64);
            let ch_str = if idx_f.is_finite() && idx_f >= 0.0 {
                let idx = idx_f as usize;
                s.chars()
                    .nth(idx)
                    .map(|c| c.to_string())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let str_id = state.strings.len();
            state.strings.push(ch_str);
            result_i64 = nanbox_string(str_id);
        }
    } else if name == "string_charCodeAt" || name == "string_char_code_at" {
        if raw_args.len() >= 2 {
            let s = state.get_string(raw_args[0]);
            let idx_val = raw_args[1];
            let idx_f = f64::from_bits(idx_val as u64);
            if idx_f.is_finite() && idx_f >= 0.0 {
                let idx = idx_f as usize;
                if let Some(c) = s.chars().nth(idx) {
                    result_i64 = ((c as u32) as f64).to_bits() as i64;
                } else {
                    result_i64 = f64::NAN.to_bits() as i64;
                }
            } else {
                result_i64 = f64::NAN.to_bits() as i64;
            }
        }
    } else if name == "string_len" || name == "array_length" {
        let arg = raw_args.first().copied().unwrap_or(0);
        if let Some(h) = state.get_handle(arg) {
            match h {
                JsHandle::Array(arr) => {
                    result_i64 = (arr.len() as f64).to_bits() as i64;
                }
                JsHandle::Uint8Array(v) => {
                    result_i64 = (v.byte_length as f64).to_bits() as i64;
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
    } else if name == "object_set_dynamic" || name == "array_set" {
        if raw_args.len() >= 3 {
            let target_handle = raw_args[0];
            let idx_val = raw_args[1];
            let val_val = raw_args[2];
            let idx_bits = idx_val as u64;
            let idx = if (idx_bits >> 48) < 0x7ff8 {
                f64::from_bits(idx_bits) as usize
            } else {
                (idx_bits & 0xFFFF_FFFF) as usize
            };
            let key_str = state.get_string(idx_val);
            let val_json = state.to_js_value(val_val);
            let val_byte = state.to_uint8(val_val);
            if let Some(h) = state.get_handle_mut(target_handle) {
                match h {
                    JsHandle::Uint8Array(v) => {
                        v.set(idx, val_byte);
                    }
                    JsHandle::Array(arr) => {
                        if idx < arr.len() {
                            arr[idx] = val_val;
                        } else if idx == arr.len() {
                            arr.push(val_val);
                        }
                    }
                    JsHandle::Json(serde_json::Value::Object(map)) => {
                        map.insert(key_str, val_json);
                    }
                    _ => {}
                }
            }
            result_i64 = val_val;
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
