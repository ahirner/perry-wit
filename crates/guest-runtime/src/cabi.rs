//! Canonical ABI memory layout, string conversion, and argument marshalling.

use core::alloc::Layout;

use crate::state::get_state;

/// Releases pending timers and stops an invocation after an uncaught guest exception.
#[no_mangle]
pub extern "C" fn cabi_check_exception() {
    let state = get_state();
    if let Some(error) = state.current_exception.as_deref() {
        state.timers.cancel_all();
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
    let area = Box::into_raw(Box::new(allocate_bytes(s.as_bytes()))) as i32;
    state.pending_return_area = Some((area, 2));
    area
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
    let area = Box::into_raw(Box::new([branch, ptr, len])) as i32;
    get_state().pending_return_area = Some((area, 3));
    area
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
        state.from_js_value(json_val)
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
    let area = Box::into_raw(Box::new(allocate_bytes(json.as_bytes()))) as i32;
    state.pending_return_area = Some((area, 2));
    area
}

/// Cleanup hook called by host post-return to reclaim Canonical ABI memory buffers.
#[no_mangle]
pub extern "C" fn cabi_post_cleanup(ret_ptr: i32) {
    get_state().pending_return_area = None;
    free_string_return_area::<2>(ret_ptr);
}

/// Reclaims a result discriminant and its selected string payload.
#[no_mangle]
pub extern "C" fn cabi_post_result_cleanup(ret_ptr: i32) {
    get_state().pending_return_area = None;
    free_string_return_area::<3>(ret_ptr);
}

pub(crate) fn free_pending_return_area(ret_ptr: i32, words: usize) {
    if words == 3 {
        free_string_return_area::<3>(ret_ptr);
    } else {
        free_string_return_area::<2>(ret_ptr);
    }
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

/// Records the boundary of static strings and handles allocated during module initialization.
#[no_mangle]
pub extern "C" fn cabi_record_init_checkpoint() {
    get_state().record_init_checkpoint();
}

/// Prepares the instance for a new invocation, clearing prior errors and reclaiming abandoned state.
#[no_mangle]
pub extern "C" fn cabi_reset_invocation_state() {
    get_state().reset_invocation_state();
}

/// Debug hook to print internal state lengths and capacities.
#[no_mangle]
pub extern "C" fn cabi_debug_state() {
    let state = get_state();
    eprintln!(
        "DEBUG: strings len={} cap={} | handles len={} cap={} | free_s={} free_h={} | roots={}",
        state.strings.len(),
        state.strings.capacity(),
        state.handles.len(),
        state.handles.capacity(),
        state.free_strings.len(),
        state.free_handles.len(),
        state.global_roots.len()
    );
}

/// Registers a live global NaN-boxed value as a root to survive reclamation.
#[no_mangle]
pub extern "C" fn cabi_register_global_root(val: i64) {
    get_state().register_root(val);
}

/// Reclaims invocation-scoped temporaries while preserving static literals and surviving roots.
#[no_mangle]
pub extern "C" fn cabi_reclaim_temporaries() {
    get_state().reclaim_temporaries();
}

/// Canonical ABI allocator with proper deallocation on zero size.
#[no_mangle]
pub unsafe extern "C" fn cabi_realloc(
    old_ptr: *mut u8,
    old_len: usize,
    align: usize,
    new_len: usize,
) -> *mut u8 {
    if new_len == 0 {
        if old_len > 0 && !old_ptr.is_null() {
            let layout = Layout::from_size_align_unchecked(old_len, align.max(1));
            std::alloc::dealloc(old_ptr, layout);
        }
        return align as *mut u8;
    }
    let pointer = if old_len == 0 || old_ptr.is_null() {
        let layout = Layout::from_size_align_unchecked(new_len, align.max(1));
        std::alloc::alloc(layout)
    } else {
        let layout = Layout::from_size_align_unchecked(old_len, align.max(1));
        std::alloc::realloc(old_ptr, layout, new_len)
    };
    if pointer.is_null() {
        core::arch::wasm32::unreachable();
    }
    pointer
}
