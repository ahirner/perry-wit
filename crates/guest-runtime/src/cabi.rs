//! Canonical ABI memory layout, string conversion, and argument marshalling.

use crate::state::get_state;

/// Stops a component invocation if guest execution left an uncaught exception.
#[no_mangle]
pub extern "C" fn cabi_check_exception() {
    if let Some(error) = get_state().current_exception.as_deref() {
        crate::io::fail_with_error(error);
    }
}

/// Imports a UTF-8 string slice from Canonical ABI memory into a nanboxed JS string value.
#[no_mangle]
pub extern "C" fn cabi_import_string(ptr: i32, len: i32) -> i64 {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let s = std::str::from_utf8(slice).unwrap_or_default();
    state.alloc_string(s)
}

/// Exports a nanboxed JS value as a Canonical ABI UTF-8 string `(ptr, len)`.
///
/// Returns an `i32` pointer to an 8-byte return area: `[u32 str_ptr, u32 str_len]`.
#[no_mangle]
pub extern "C" fn cabi_export_string(val: i64) -> i32 {
    let state = get_state();
    let s = state.get_string(val);
    Box::into_raw(Box::new(allocate_bytes(s.as_bytes()))) as i32
}

/// Exports a nanboxed JS value as a Canonical ABI `result<string, string>`.
///
/// Returns an `i32` pointer to a 12-byte return area:
/// `[u32 discriminant, u32 str_ptr, u32 str_len]` (0 = ok, 1 = err).
#[no_mangle]
pub extern "C" fn cabi_export_result_string(val: i64) -> i32 {
    let value = get_state().to_js_value(val);
    let (branch, payload) = match value.get("ok").and_then(serde_json::Value::as_bool) {
        Some(true) => (0, value.get("value")),
        Some(false) => (1, value.get("error")),
        None => {
            crate::io::fail_with_error("WIT result requires an object with a boolean 'ok' field")
        }
    };
    let payload = payload
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| {
            crate::io::fail_with_error("WIT result requires a string payload in 'value' or 'error'")
        });
    let [ptr, len] = allocate_bytes(payload.as_bytes());
    Box::into_raw(Box::new([branch, ptr, len])) as i32
}

fn allocate_bytes(bytes: &[u8]) -> [u32; 2] {
    let len = bytes.len() as u32;
    let ptr = Box::into_raw(bytes.to_vec().into_boxed_slice()) as *mut u8 as u32;
    [ptr, len]
}

/// Imports a UTF-8 JSON payload from Canonical ABI memory into a structured JS object handle.
#[no_mangle]
pub extern "C" fn cabi_import_json(ptr: i32, len: i32) -> i64 {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(slice) {
        let id = state.alloc_handle(crate::state::JsHandle::Json(json_val));
        crate::nanbox::nanbox_pointer(id)
    } else {
        let s = std::str::from_utf8(slice).unwrap_or_default();
        state.alloc_string(s)
    }
}

/// Exports a nanboxed JS object/value as a Canonical ABI JSON string `(ptr, len)`.
///
/// Returns an `i32` pointer to an 8-byte return area: `[u32 json_ptr, u32 json_len]`.
#[no_mangle]
pub extern "C" fn cabi_export_json(val: i64) -> i32 {
    let state = get_state();
    let json = state.stringify(val);
    Box::into_raw(Box::new(allocate_bytes(json.as_bytes()))) as i32
}

/// Cleanup hook called by host post-return to reclaim Canonical ABI memory buffers.
#[no_mangle]
pub extern "C" fn cabi_post_cleanup(ret_ptr: i32) {
    free_string_return_area::<2>(ret_ptr);
}

/// Reclaims a result discriminant and its selected string payload.
#[no_mangle]
pub extern "C" fn cabi_post_result_cleanup(ret_ptr: i32) {
    free_string_return_area::<3>(ret_ptr);
}

fn free_string_return_area<const WORDS: usize>(ret_ptr: i32) {
    if ret_ptr == 0 {
        return;
    }
    unsafe {
        let area = Box::from_raw(ret_ptr as *mut [u32; WORDS]);
        let (ptr, len) = (area[WORDS - 2] as *mut u8, area[WORDS - 1] as usize);
        if len != 0 {
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len)));
        }
    }
}
