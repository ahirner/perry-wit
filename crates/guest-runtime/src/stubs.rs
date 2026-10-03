// -----------------------------------------------------------------------------
// Auto-generated runtime function stubs (208 stubs for static link compatibility)
// -----------------------------------------------------------------------------
use crate::nanbox::{
    nanbox_pointer, nanbox_string, POINTER_TAG, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE,
    TAG_UNDEFINED,
};
use crate::objects::{object_get, object_get_dynamic, object_set, object_set_dynamic};
use crate::state::{get_state, JsHandle};

#[no_mangle]
pub(crate) extern "C" fn console_warn(_a: i64) {}
#[no_mangle]
pub(crate) extern "C" fn string_concat(a: i64, b: i64) -> i64 {
    let state = get_state();
    let mut units = Vec::new();
    units.extend_from_slice(state.string_units(a).as_ref());
    units.extend_from_slice(state.string_units(b).as_ref());
    state.alloc_string_units(units)
}
#[no_mangle]
pub(crate) extern "C" fn js_add(a: i64, b: i64) -> i64 {
    let args = [a, b];
    if args
        .iter()
        .all(|&value| !matches!((value as u64) >> 48, STRING_TAG | POINTER_TAG))
    {
        let [left, right] = args.map(|value| match value as u64 {
            TAG_TRUE => 1.0,
            TAG_FALSE | TAG_NULL => 0.0,
            TAG_UNDEFINED => f64::NAN,
            bits => f64::from_bits(bits),
        });
        (left + right).to_bits() as i64
    } else {
        string_concat(a, b)
    }
}
#[no_mangle]
pub(crate) extern "C" fn string_eq(a: i64, b: i64) -> i32 {
    let state = get_state();
    if crate::equality::equal(state, a, b, false) {
        1
    } else {
        0
    }
}
#[no_mangle]
pub(crate) extern "C" fn string_len(value: i64) -> i64 {
    (get_state().string_units(value).len() as f64).to_bits() as i64
}
#[no_mangle]
pub(crate) extern "C" fn jsvalue_to_string(value: i64) -> i64 {
    let state = get_state();
    let units = state.string_units(value).into_owned();
    state.alloc_string_units(units)
}
#[no_mangle]
pub(crate) extern "C" fn jsvalue_to_template_string(value: i64) -> i64 {
    jsvalue_to_string(value)
}
#[no_mangle]
pub(crate) extern "C" fn is_truthy(val: i64) -> i32 {
    let bits = val as u64;
    if bits == TAG_UNDEFINED || bits == TAG_NULL || bits == TAG_FALSE {
        return 0;
    }
    if bits == TAG_TRUE {
        return 1;
    }
    if (bits >> 48) == STRING_TAG {
        let state = get_state();
        let id = (bits & 0xFFFF_FFFF) as usize;
        return if state.strings.get(id).map_or(false, |s| !s.is_empty()) {
            1
        } else {
            0
        };
    }
    if (bits >> 48) == POINTER_TAG {
        return 1;
    }
    let num = f64::from_bits(bits);
    if num == 0.0 || num.is_nan() {
        0
    } else {
        1
    }
}
#[no_mangle]
pub(crate) extern "C" fn js_strict_eq(a: i64, b: i64) -> i32 {
    let state = get_state();
    if crate::equality::equal(state, a, b, false) {
        1
    } else {
        0
    }
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
pub(crate) extern "C" fn js_typeof(val: i64) -> i64 {
    let state = get_state();
    let bits = val as u64;
    let type_name = if bits == TAG_UNDEFINED {
        "undefined"
    } else if bits == TAG_NULL {
        "object"
    } else if bits == TAG_TRUE || bits == TAG_FALSE {
        "boolean"
    } else if (bits >> 48) == STRING_TAG {
        "string"
    } else if (bits >> 48) == POINTER_TAG {
        let id = (bits & 0xFFFF_FFFF) as usize;
        match state.handles.get(id) {
            Some(JsHandle::Object(_)) => "object",
            Some(JsHandle::Array(_)) => "object",
            Some(JsHandle::Uint8Array(_)) => "object",
            Some(JsHandle::Response { .. } | JsHandle::Headers(_)) => "object",
            Some(JsHandle::Date(_)) => "object",
            Some(JsHandle::Cell(_) | JsHandle::Promise(_)) => "object",
            Some(JsHandle::BufferedHttp(_)) => "object",
            Some(JsHandle::Null) => "object",
            Some(JsHandle::Closure(_) | JsHandle::PromiseResolver(_)) => "function",
            None => "undefined",
        }
    } else {
        "number"
    };
    state.alloc_string(type_name)
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
pub(crate) extern "C" fn js_mod(a: i64, b: i64) -> i64 {
    let state = get_state();
    (state.to_number(a) % state.to_number(b)).to_bits() as i64
}
#[no_mangle]
pub(crate) extern "C" fn is_null_or_undefined(_a: i64) -> i32 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_new() -> i64 {
    let state = get_state();
    let h_id = state.alloc_handle(JsHandle::Array(Vec::new()));
    nanbox_pointer(h_id)
}
#[no_mangle]
pub(crate) extern "C" fn array_push(target: i64, item: i64) -> i64 {
    let state = get_state();
    if let Some(JsHandle::Array(arr)) = state.get_handle_mut(target) {
        arr.push(item);
        return target;
    }
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_pop(target: i64) -> i64 {
    let state = get_state();
    if let Some(JsHandle::Array(arr)) = state.get_handle_mut(target) {
        return arr.pop().unwrap_or(TAG_UNDEFINED as i64);
    }
    TAG_UNDEFINED as i64
}
#[no_mangle]
pub(crate) extern "C" fn array_get(target: i64, index_val: i64) -> i64 {
    object_get_dynamic(target, index_val)
}
#[no_mangle]
pub(crate) extern "C" fn array_set(target: i64, key: i64, val: i64) {
    object_set_dynamic(target, key, val);
}
#[no_mangle]
pub(crate) extern "C" fn array_length(target: i64) -> i64 {
    let state = get_state();
    if let Some(h) = state.get_handle(target) {
        let len = match h {
            JsHandle::Array(arr) => arr.len(),
            JsHandle::Uint8Array(v) => v.byte_length,
            _ => 0,
        };
        return (len as f64).to_bits() as i64;
    }
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
pub(crate) extern "C" fn array_join(target: i64, sep: i64) -> i64 {
    let state = get_state();
    let separator = if sep as u64 == TAG_UNDEFINED {
        vec![b',' as u16]
    } else {
        state.string_units(sep).into_owned()
    };
    let mut joined = Vec::new();
    if let Some(JsHandle::Array(items)) = state.get_handle(target) {
        for (index, &item) in items.iter().enumerate() {
            if index > 0 {
                joined.extend_from_slice(&separator);
            }
            if !matches!(item as u64, TAG_UNDEFINED | TAG_NULL) {
                joined.extend_from_slice(&state.string_units(item));
            }
        }
    }
    state.alloc_string_units(joined)
}
#[no_mangle]
pub(crate) extern "C" fn array_index_of(_a: i64, _b: i64) -> i64 {
    0
}
#[no_mangle]
pub(crate) extern "C" fn array_includes(target: i64, search_elem: i64) -> i32 {
    let state = get_state();
    if let Some(JsHandle::Array(arr)) = state.get_handle(target) {
        for &item in arr {
            if crate::equality::equal(state, item, search_elem, false) {
                return 1;
            }
        }
    }
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
pub(crate) extern "C" fn string_charAt(value: i64, index: i64) -> i64 {
    let state = get_state();
    let units = state.string_code_unit(value, index).into_iter().collect();
    state.alloc_string_units(units)
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
pub(crate) extern "C" fn string_includes(target: i64, search: i64) -> i32 {
    let state = get_state();
    if let Some(JsHandle::Array(_)) = state.get_handle(target) {
        return array_includes(target, search);
    }
    let target = state.string_units(target);
    let search = state.string_units(search);
    i32::from(search.is_empty() || target.windows(search.len()).any(|part| part == &*search))
}
#[no_mangle]
pub(crate) extern "C" fn string_startsWith(target: i64, search: i64) -> i32 {
    let state = get_state();
    i32::from(
        state
            .string_units(target)
            .starts_with(&state.string_units(search)),
    )
}
#[no_mangle]
pub(crate) extern "C" fn string_endsWith(target: i64, search: i64) -> i32 {
    let state = get_state();
    i32::from(
        state
            .string_units(target)
            .ends_with(&state.string_units(search)),
    )
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
pub(crate) extern "C" fn class_get_field(target: i64, key: i64) -> i64 {
    object_get(target, key)
}
#[no_mangle]
pub(crate) extern "C" fn class_set_field(target: i64, key: i64, val: i64) {
    object_set(target, key, val);
}
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
pub(crate) extern "C" fn json_parse(val: i64) -> i64 {
    let state = get_state();
    let s = state.get_string(val);
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&s) {
        state.from_js_value(v)
    } else {
        TAG_UNDEFINED as i64
    }
}
#[no_mangle]
pub(crate) extern "C" fn json_stringify(val: i64) -> i64 {
    let state = get_state();
    match state.try_stringify(val) {
        Ok(json) => state.alloc_string(&json),
        Err(error) => {
            state.current_exception = Some(error.into());
            TAG_UNDEFINED as i64
        }
    }
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
        state.alloc_string(&err)
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
        state.alloc_string(&s)
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
        let id = state.alloc_handle(JsHandle::Uint8Array(
            crate::buffer::Uint8ArrayView::from_bytes(Vec::new()),
        ));
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
    let integer = state.to_number(size).trunc();
    let length = if integer.is_nan() { 0.0 } else { integer };
    if !(0.0..=isize::MAX as f64).contains(&length) {
        state.current_exception = Some("RangeError: Invalid typed array length".into());
        return TAG_UNDEFINED as i64;
    }
    let Ok(view) = crate::buffer::Uint8ArrayView::try_new(length as usize) else {
        state.current_exception = Some("RangeError: Unable to allocate typed array".into());
        return TAG_UNDEFINED as i64;
    };
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

#[no_mangle]
pub(crate) extern "C" fn fs_read_file_sync(path: i64) -> i64 {
    crate::filesystem::fs_read_file_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_read_file_binary(path: i64) -> i64 {
    crate::filesystem::fs_read_file_binary(path)
}

#[no_mangle]
pub(crate) extern "C" fn fs_write_file_sync(path: i64, content: i64) -> i64 {
    crate::filesystem::fs_write_file_sync(path, content, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_readdir_sync(path: i64) -> i64 {
    crate::filesystem::fs_readdir_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_stat_sync(path: i64) -> i64 {
    crate::filesystem::fs_stat_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_unlink_sync(path: i64) -> i64 {
    crate::filesystem::fs_unlink_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_mkdir_sync(path: i64) -> i64 {
    crate::filesystem::fs_mkdir_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_rmdir_sync(path: i64) -> i64 {
    crate::filesystem::fs_rmdir_sync(path, TAG_UNDEFINED as i64)
}

#[no_mangle]
pub(crate) extern "C" fn fs_exists_sync(path: i64) -> i64 {
    crate::filesystem::fs_exists_sync(path)
}

#[no_mangle]
pub(crate) extern "C" fn js_native_module_named_esm_export_value(
    _module: f64,
    _property: f64,
) -> f64 {
    f64::from_bits(crate::nanbox::TAG_UNDEFINED)
}
