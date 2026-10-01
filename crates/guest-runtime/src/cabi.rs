//! Canonical ABI memory layout, string conversion, and argument marshalling.

use std::alloc::Layout;

use crate::nanbox::nanbox_string;
use crate::state::get_state;

/// Imports a UTF-8 string slice from Canonical ABI memory into a nanboxed JS string value.
#[no_mangle]
pub extern "C" fn cabi_import_string(ptr: i32, len: i32) -> i64 {
    let state = get_state();
    let slice = unsafe { std::slice::from_raw_parts(ptr as *const u8, len as usize) };
    let s = std::str::from_utf8(slice).unwrap_or_default();
    let id = state.strings.len();
    state.strings.push(s.to_string());
    nanbox_string(id)
}

/// Exports a nanboxed JS value as a Canonical ABI UTF-8 string `(ptr, len)`.
///
/// Returns an `i32` pointer to an 8-byte return area: `[u32 str_ptr, u32 str_len]`.
#[no_mangle]
pub extern "C" fn cabi_export_string(val: i64) -> i32 {
    let state = get_state();
    let s = state.get_string(val);
    let bytes = s.into_bytes();
    let str_len = bytes.len() as u32;

    let str_ptr = if str_len > 0 {
        let mut b = bytes.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p as u32
    } else {
        1
    };

    let mut ret_area = Box::new([0u32; 2]);
    ret_area[0] = str_ptr;
    ret_area[1] = str_len;
    Box::into_raw(ret_area) as *mut u8 as i32
}

/// Exports a nanboxed JS value as a Canonical ABI `result<string, string>`.
///
/// Returns an `i32` pointer to a 12-byte return area:
/// `[u32 discriminant, u32 str_ptr, u32 str_len]` (0 = ok, 1 = err).
#[no_mangle]
pub extern "C" fn cabi_export_result_string(val: i64, is_err: i32) -> i32 {
    let state = get_state();
    let s = state.get_string(val);
    let bytes = s.into_bytes();
    let str_len = bytes.len() as u32;

    let str_ptr = if str_len > 0 {
        let mut b = bytes.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p as u32
    } else {
        1
    };

    let mut ret_area = Box::new([0u32; 3]);
    ret_area[0] = is_err as u32;
    ret_area[1] = str_ptr;
    ret_area[2] = str_len;
    Box::into_raw(ret_area) as *mut u8 as i32
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
        let id = state.strings.len();
        state.strings.push(s.to_string());
        nanbox_string(id)
    }
}

/// Exports a nanboxed JS object/value as a Canonical ABI JSON string `(ptr, len)`.
///
/// Returns an `i32` pointer to an 8-byte return area: `[u32 json_ptr, u32 json_len]`.
#[no_mangle]
pub extern "C" fn cabi_export_json(val: i64) -> i32 {
    let state = get_state();
    let js_val = state.to_js_value(val);
    let bytes = serde_json::to_vec(&js_val).unwrap_or_else(|_| b"null".to_vec());
    let json_len = bytes.len() as u32;

    let json_ptr = if json_len > 0 {
        let mut b = bytes.into_boxed_slice();
        let p = b.as_mut_ptr();
        std::mem::forget(b);
        p as u32
    } else {
        1
    };

    let mut ret_area = Box::new([0u32; 2]);
    ret_area[0] = json_ptr;
    ret_area[1] = json_len;
    Box::into_raw(ret_area) as *mut u8 as i32
}

/// Cleanup hook called by host post-return to reclaim Canonical ABI memory buffers.
#[no_mangle]
pub extern "C" fn cabi_post_cleanup(ret_ptr: i32) {
    if ret_ptr != 0 {
        unsafe {
            let ptr_slice = std::slice::from_raw_parts_mut(ret_ptr as *mut u32, 2);
            let str_ptr = ptr_slice[0] as *mut u8;
            let str_len = ptr_slice[1] as usize;
            if !str_ptr.is_null() && str_ptr as usize != 1 && str_len > 0 {
                let layout = Layout::from_size_align_unchecked(str_len, 1);
                std::alloc::dealloc(str_ptr, layout);
            }
            let ret_layout = Layout::from_size_align_unchecked(8, 4);
            std::alloc::dealloc(ret_ptr as *mut u8, ret_layout);
        }
    }
}
