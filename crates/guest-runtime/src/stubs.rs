// -----------------------------------------------------------------------------
// Auto-generated runtime function stubs (208 stubs for static link compatibility)
// -----------------------------------------------------------------------------
use crate::nanbox::{nanbox_pointer, nanbox_string, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub(crate) extern "C" fn console_warn(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn string_concat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn js_add(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_eq(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_len(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn jsvalue_to_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn jsvalue_to_template_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn is_truthy(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn js_strict_eq(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_floor(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_ceil(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_round(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_abs(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_sqrt(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_pow(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_random() -> i64 {
    crate::random::math_random()
}
#[no_mangle]
pub(crate) extern "C" fn math_log(_a: i64) -> i64 {
    0
}

#[no_mangle]
pub(crate) extern "C" fn js_typeof(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_min(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_max(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn parse_int(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn parse_float(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn js_mod(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn is_null_or_undefined(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_new() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_set(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_get_dynamic(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_set_dynamic(target: i64, key: i64, val: i64) {
    let state = get_state();
    let idx_bits = key as u64;
    let idx = if (idx_bits >> 48) < 0x7ff8 {
        f64::from_bits(idx_bits) as usize
    } else {
        (idx_bits & 0xFFFF_FFFF) as usize
    };
    let key_str = state.get_string(key);
    let val_json = state.to_js_value(val);
    let val_byte = state.to_uint8(val);
    let element_index = state.element_index(key);
    if let Some(h) = state.get_handle_mut(target) {
        match h {
            JsHandle::Uint8Array(v) => {
                if let Some(index) = element_index {
                    v.set(index, val_byte);
                }
            }
            JsHandle::Array(arr) => {
                if idx < arr.len() {
                    arr[idx] = val;
                } else if idx == arr.len() {
                    arr.push(val);
                }
            }
            JsHandle::Json(serde_json::Value::Object(map)) => {
                map.insert(key_str, val_json);
            }
            _ => {}
        }
    }
}
#[no_mangle]
pub(crate) extern "C" fn object_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn object_delete_dynamic(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn object_keys(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_entries(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_has_property(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn object_assign(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_new() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_push(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_pop(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn array_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_slice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_splice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_shift(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_unshift(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn array_join(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_index_of(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_includes(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_concat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_reverse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_flat(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_is_array(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_from(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_push_spread(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_charAt(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_substring(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_indexOf(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_slice(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_toLowerCase(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_toUpperCase(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_trim(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_includes(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_startsWith(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_endsWith(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_replace(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_split(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_fromCharCode(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_padStart(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_padEnd(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_repeat(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn string_match(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_log2(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn math_log10(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_set_capture(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_call_0(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_call_1(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_call_2(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_call_3(_a: i64, _b: i64, _c: i64, _d: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn closure_call_spread(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_map(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_filter(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_forEach(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn array_reduce(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_find(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_find_index(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_sort(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_some(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_every(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn class_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn class_set_method(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn class_call_method(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn class_get_field(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn class_set_field(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn class_set_static(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn class_get_static(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn class_instanceof(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn json_parse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn json_stringify(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_new() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn map_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn map_size(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_clear(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn map_entries(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_keys(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn map_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_new() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_new_from_array(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_add(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn set_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn set_size(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_clear(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn set_values(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn error_new(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn error_message(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn regexp_new(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn regexp_test(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn number_coerce(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn is_nan(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn is_finite(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn console_log_multi(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn class_set_parent(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn try_start() {}
#[no_mangle]
pub(crate) extern "C" fn try_end() {}
#[no_mangle]
pub(crate) extern "C" fn throw_value(value: i64) {
    let state = crate::state::get_state();
    state.current_exception = Some(state.get_string(value));
}
#[no_mangle]
pub(crate) extern "C" fn has_exception() -> i32 {
    let state = crate::state::get_state();
    if state.current_exception.is_some() {
        1
    } else {
        0
    }
}
#[no_mangle]
pub(crate) extern "C" fn get_exception() -> i64 {
    let state = crate::state::get_state();
    if let Some(err) = state.current_exception.take() {
        let str_id = state.strings.len();
        state.strings.push(err);
        crate::nanbox::nanbox_string(str_id)
    } else {
        crate::nanbox::TAG_UNDEFINED as i64
    }
}
#[no_mangle]
pub(crate) extern "C" fn url_parse(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_href(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_pathname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_hostname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_port(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_search(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_hash(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_origin(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_protocol(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn url_get_search_params(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn searchparams_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn searchparams_has(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn searchparams_set(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn searchparams_append(_a: i64, _b: i64, _c: i64) {}
#[no_mangle]
pub(crate) extern "C" fn searchparams_delete(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn searchparams_to_string(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn crypto_random_uuid() -> i64 {
    crate::random::crypto_random_uuid()
}
#[no_mangle]
pub(crate) extern "C" fn crypto_random_bytes(len: i64) -> i64 {
    crate::random::crypto_random_bytes(len)
}
#[no_mangle]
pub(crate) extern "C" fn path_join(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn path_dirname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn path_basename(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn path_extname(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn path_resolve(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn os_platform() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn process_argv() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn process_cwd() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_alloc(size: i64) -> i64 {
    uint8array_new(size)
}
#[no_mangle]
pub(crate) extern "C" fn buffer_from_string(s_val: i64, _encoding: i64) -> i64 {
    let state = get_state();
    let s = state.get_string(s_val);
    let view = crate::buffer::Uint8ArrayView::from_bytes(s.into_bytes());
    let id = state.alloc_handle(JsHandle::Uint8Array(view));
    nanbox_pointer(id)
}
#[no_mangle]
pub(crate) extern "C" fn buffer_to_string(handle: i64, _encoding: i64) -> i64 {
    let state = get_state();
    if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle) {
        let bytes = v.to_vec();
        let s = String::from_utf8_lossy(&bytes).into_owned();
        let id = state.strings.len();
        state.strings.push(s);
        nanbox_string(id)
    } else {
        nanbox_string(0)
    }
}
#[no_mangle]
pub(crate) extern "C" fn buffer_get(handle: i64, idx: i64) -> i64 {
    uint8array_get(handle, idx)
}
#[no_mangle]
pub(crate) extern "C" fn buffer_set(handle: i64, idx: i64, val: i64) {
    uint8array_set(handle, idx, val);
}
#[no_mangle]
pub(crate) extern "C" fn buffer_length(handle: i64) -> i64 {
    uint8array_length(handle)
}
#[no_mangle]
pub(crate) extern "C" fn buffer_slice(handle: i64, start: i64, end: i64) -> i64 {
    let state = get_state();
    let start_idx = state.to_number(start);
    let end_bits = end as u64;
    let end_idx = if end_bits != TAG_UNDEFINED {
        Some(state.to_number(end))
    } else {
        None
    };
    if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle).cloned() {
        let subview = v.subview(start_idx, end_idx);
        let id = state.alloc_handle(JsHandle::Uint8Array(subview));
        nanbox_pointer(id)
    } else {
        let id = state.alloc_handle(JsHandle::Uint8Array(crate::buffer::Uint8ArrayView::new(0)));
        nanbox_pointer(id)
    }
}
#[no_mangle]
pub(crate) extern "C" fn buffer_concat(arr_handle: i64) -> i64 {
    let state = get_state();
    let mut all_bytes = Vec::new();
    if let Some(JsHandle::Array(items)) = state.get_handle(arr_handle).cloned() {
        for item in items {
            if let Some(JsHandle::Uint8Array(v)) = state.get_handle(item) {
                all_bytes.extend(v.to_vec());
            }
        }
    }
    let view = crate::buffer::Uint8ArrayView::from_bytes(all_bytes);
    let id = state.alloc_handle(JsHandle::Uint8Array(view));
    nanbox_pointer(id)
}
#[no_mangle]
pub(crate) extern "C" fn uint8array_new(size: i64) -> i64 {
    let state = get_state();
    if state.get_handle(size).is_some() {
        return uint8array_from(size);
    }
    let size_f = state.to_number(size);
    let len = if size_f.is_finite() && size_f > 0.0 {
        size_f as usize
    } else {
        0
    };
    let view = crate::buffer::Uint8ArrayView::new(len);
    let id = state.alloc_handle(JsHandle::Uint8Array(view));
    nanbox_pointer(id)
}
#[no_mangle]
pub(crate) extern "C" fn uint8array_from(val: i64) -> i64 {
    let state = get_state();
    let mut bytes = Vec::new();
    if let Some(h) = state.get_handle(val).cloned() {
        match h {
            JsHandle::Uint8Array(v) => {
                bytes = v.to_vec();
            }
            JsHandle::Array(arr) => {
                bytes.reserve(arr.len());
                for elem in arr {
                    bytes.push(state.to_uint8(elem));
                }
            }
            JsHandle::Json(serde_json::Value::Array(arr)) => {
                bytes.reserve(arr.len());
                for item in arr {
                    let value = state.from_js_value(item);
                    bytes.push(state.to_uint8(value));
                }
            }
            _ => {}
        }
    } else {
        let s = state.get_string(val);
        bytes = s.into_bytes();
    }
    let view = crate::buffer::Uint8ArrayView::from_bytes(bytes);
    let id = state.alloc_handle(JsHandle::Uint8Array(view));
    nanbox_pointer(id)
}
#[no_mangle]
pub(crate) extern "C" fn uint8array_length(handle: i64) -> i64 {
    let state = get_state();
    if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle) {
        (v.byte_length as f64).to_bits() as i64
    } else {
        0
    }
}
#[no_mangle]
pub(crate) extern "C" fn uint8array_get(handle: i64, idx: i64) -> i64 {
    let state = get_state();
    if let Some(JsHandle::Uint8Array(v)) = state.get_handle(handle) {
        if let Some(b) = state.element_index(idx).and_then(|index| v.get(index)) {
            (b as f64).to_bits() as i64
        } else {
            TAG_UNDEFINED as i64
        }
    } else {
        TAG_UNDEFINED as i64
    }
}
#[no_mangle]
pub(crate) extern "C" fn uint8array_set(handle: i64, idx: i64, val: i64) {
    let state = get_state();
    if let (Some(JsHandle::Uint8Array(view)), Some(index)) =
        (state.get_handle(handle), state.element_index(idx))
    {
        view.set(index, state.to_uint8(val));
    }
}
#[no_mangle]
pub(crate) extern "C" fn set_timeout(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn set_interval(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn clear_timeout(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn clear_interval(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn response_status(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn response_ok(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn response_headers_get(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn response_url(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_copy(_a: i64, _b: i64, _c: i64, _d: i64, _e: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_write(_a: i64, _b: i64, _c: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_equals(_a: i64, _b: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_is_buffer(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn buffer_byte_length(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn crypto_sha256(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn crypto_md5(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn path_is_absolute(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn fetch_url(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn fetch_with_options(_a: i64, _b: i64, _c: i64, _d: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn response_json(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn response_text(_a: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn promise_new() -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn promise_resolve(_a: i64, _b: i64) {}
#[no_mangle]
pub(crate) extern "C" fn promise_then(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn await_promise(_a: i64) -> i64 {
    0
}
