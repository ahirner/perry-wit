//! Pure runtime dispatch (`mem_call_pure`, `mem_call_i32`).
//!
//! Contains zero capability dependencies: no WASI HTTP, no WASI clocks,
//! only pure memory operations, JSON manipulation, arrays, objects, strings,
//! and standard console/exit I/O.

use crate::io::{fail_with_error, print_stdout};
use crate::nanbox::{
    nanbox_pointer, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED,
};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub extern "C" fn mem_call_pure(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
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
    } else if name == "array_join" {
        let arr_handle = raw_args.first().copied().unwrap_or(0);
        let sep = raw_args.get(1).copied().unwrap_or(0);
        result_i64 = crate::stubs::array_join(arr_handle, sep);
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
        if let [handle, index, ..] = raw_args.as_slice() {
            result_i64 = crate::stubs::uint8array_get(*handle, *index);
        }
    } else if name == "uint8array_set" || name == "buffer_set" {
        if let [handle, index, value, ..] = raw_args.as_slice() {
            crate::stubs::uint8array_set(*handle, *index, *value);
        }
    } else if name == "buffer_slice" {
        if let Some(&handle) = raw_args.first() {
            let start = raw_args.get(1).copied().unwrap_or(0);
            let end = raw_args.get(2).copied().unwrap_or(TAG_UNDEFINED as i64);
            result_i64 = crate::stubs::buffer_slice(handle, start, end);
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
                        if let Some(b) = state.element_index(idx_val).and_then(|index| v.get(index))
                        {
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
                        } else {
                            result_i64 = TAG_UNDEFINED as i64;
                        }
                    }
                    _ => {}
                }
            } else if (target_handle as u64) >> 48 == STRING_TAG {
                let unit = state
                    .element_index(idx_val)
                    .and_then(|index| state.string_units(target_handle).get(index).copied());
                result_i64 = unit.map_or(TAG_UNDEFINED as i64, |unit| {
                    state.alloc_string_units(vec![unit])
                });
            }
        }
    } else if name == "string_charAt" || name == "string_char_at" {
        let value = raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
        let index = raw_args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
        result_i64 = crate::stubs::string_charAt(value, index);
    } else if matches!(
        name.as_str(),
        "charCodeAt" | "string_charCodeAt" | "string_char_code_at"
    ) {
        let value = raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
        let index = raw_args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
        result_i64 = state
            .string_code_unit(value, index)
            .map_or(f64::NAN, f64::from)
            .to_bits() as i64;
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
                    let len = state.string_units(arg).len();
                    result_i64 = (len as f64).to_bits() as i64;
                }
            }
        } else {
            let len = state.string_units(arg).len();
            result_i64 = (len as f64).to_bits() as i64;
        }
    } else if name == "object_new" {
        let h_id = state.alloc_handle(JsHandle::Json(serde_json::Value::Object(
            serde_json::Map::new(),
        )));
        result_i64 = nanbox_pointer(h_id);
    } else if name == "object_set" || name == "class_set_field" {
        if raw_args.len() >= 3 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            let val_json = state.object_property_value(target_handle, raw_args[2]);
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
            let val_json = state.object_property_value(target_handle, val_val);
            let val_byte = state.to_uint8(val_val);
            let element_index = state.element_index(idx_val);
            if let Some(h) = state.get_handle_mut(target_handle) {
                match h {
                    JsHandle::Uint8Array(v) => {
                        if let Some(index) = element_index {
                            v.set(index, val_byte);
                        }
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
            result_i64 = crate::stubs::object_assign(raw_args[0], raw_args[1]);
        }
    } else if name == "object_get" || name == "class_get_field" {
        if raw_args.len() >= 2 {
            result_i64 = crate::stubs::object_get(raw_args[0], raw_args[1]);
        }
    } else if name == "object_keys" {
        let target_handle = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
            state.get_handle(target_handle).cloned()
        {
            let keys: Vec<i64> = map.keys().map(|k| state.alloc_string(k)).collect();
            let arr_id = state.alloc_handle(JsHandle::Array(keys));
            result_i64 = nanbox_pointer(arr_id);
        }
    } else if name == "object_values" {
        let target_handle = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
            state.get_handle(target_handle).cloned()
        {
            let values: Vec<i64> = map
                .values()
                .cloned()
                .map(|v| state.from_js_value(v))
                .collect();
            let arr_id = state.alloc_handle(JsHandle::Array(values));
            result_i64 = nanbox_pointer(arr_id);
        }
    } else if name == "object_entries" {
        let target_handle = raw_args.first().copied().unwrap_or(0);
        if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
            state.get_handle(target_handle).cloned()
        {
            let entries: Vec<i64> = map
                .iter()
                .map(|(k, v)| {
                    let k_val = state.alloc_string(k);
                    let v_val = state.from_js_value(v.clone());
                    let pair_id = state.alloc_handle(JsHandle::Array(vec![k_val, v_val]));
                    nanbox_pointer(pair_id)
                })
                .collect();
            let arr_id = state.alloc_handle(JsHandle::Array(entries));
            result_i64 = nanbox_pointer(arr_id);
        }
    } else if name == "object_has_property" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle(target_handle)
            {
                result_i64 = if map.contains_key(&key_str) {
                    TAG_TRUE as i64
                } else {
                    TAG_FALSE as i64
                };
            }
        }
    } else if name == "object_delete" || name == "object_delete_dynamic" {
        if raw_args.len() >= 2 {
            let target_handle = raw_args[0];
            let key_str = state.get_string(raw_args[1]);
            if let Some(JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle_mut(target_handle)
            {
                map.remove(&key_str);
            }
            result_i64 = TAG_TRUE as i64;
        }
    } else if name == "json_stringify" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let json_str = state.stringify(arg);
        result_i64 = state.alloc_string(&json_str);
    } else if name == "json_parse" {
        let arg = raw_args.first().copied().unwrap_or(0);
        let s = state.get_string(arg);
        let val_json: serde_json::Value = serde_json::from_str(&s)
            .unwrap_or_else(|error| fail_with_error(&format!("JSON parse error: {error}")));
        result_i64 = state.from_js_value(val_json);
    } else if name == "js_typeof" {
        let arg = raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
        result_i64 = crate::stubs::js_typeof(arg);
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
                let mut units = state.string_units(args[0]).into_owned();
                units.extend_from_slice(&state.string_units(args[1]));
                result_i64 = state.alloc_string_units(units);
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
