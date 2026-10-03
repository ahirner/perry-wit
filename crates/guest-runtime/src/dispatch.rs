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

fn dispatch_filesystem(name: &str, raw_args: &[i64]) -> Option<i64> {
    match name {
        "fs_read_file_sync" | "readFileSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_read_file_sync(path, options))
        }
        "fs_read_file_binary" | "readFileBuffer" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            Some(crate::filesystem::fs_read_file_binary(path))
        }
        "fs_write_file_sync" | "writeFileSync" => {
            let args = if raw_args.len() >= 3
                && matches!(raw_args[0] as u64, TAG_UNDEFINED | TAG_NULL | 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
            let content = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            let options = args.get(2).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_write_file_sync(
                path, content, options,
            ))
        }
        "fs_readdir_sync" | "readdirSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_readdir_sync(path, options))
        }
        "fs_stat_sync" | "statSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_stat_sync(path, options))
        }
        "fs_unlink_sync" | "unlinkSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_unlink_sync(path, options))
        }
        "fs_mkdir_sync" | "mkdirSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(TAG_UNDEFINED as i64);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_mkdir_sync(path, options))
        }
        "fs_rmdir_sync" | "rmdirSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            let options = args.get(1).copied().unwrap_or(TAG_UNDEFINED as i64);
            Some(crate::filesystem::fs_rmdir_sync(path, options))
        }
        "fs_exists_sync" | "existsSync" => {
            let args = if raw_args.len() >= 2
                && (raw_args[0] == TAG_UNDEFINED as i64
                    || raw_args[0] == TAG_NULL as i64
                    || raw_args[0] == 0)
            {
                &raw_args[1..]
            } else {
                raw_args
            };
            let path = args.first().copied().unwrap_or(0);
            Some(crate::filesystem::fs_exists_sync(path))
        }
        "isFile" => {
            let state = crate::state::get_state();
            let target = raw_args.first().copied().unwrap_or(0);
            if let Some(crate::state::JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle(target)
            {
                if let Some(serde_json::Value::Bool(b)) = map.get("isFile") {
                    return Some(if *b {
                        crate::nanbox::TAG_TRUE as i64
                    } else {
                        crate::nanbox::TAG_FALSE as i64
                    });
                }
            }
            Some(crate::nanbox::TAG_FALSE as i64)
        }
        "isDirectory" => {
            let state = crate::state::get_state();
            let target = raw_args.first().copied().unwrap_or(0);
            if let Some(crate::state::JsHandle::Json(serde_json::Value::Object(map))) =
                state.get_handle(target)
            {
                if let Some(serde_json::Value::Bool(b)) = map.get("isDirectory") {
                    return Some(if *b {
                        crate::nanbox::TAG_TRUE as i64
                    } else {
                        crate::nanbox::TAG_FALSE as i64
                    });
                }
            }
            Some(crate::nanbox::TAG_FALSE as i64)
        }
        _ => None,
    }
}

fn dispatch_env(name: &str, raw_args: &[i64]) -> Option<i64> {
    match name {
        "process_env" => Some(crate::environment::process_env()),
        "process_env_get" => {
            let key = if raw_args.len() >= 2 {
                raw_args[1]
            } else {
                raw_args.first().copied().unwrap_or(TAG_UNDEFINED as i64)
            };
            Some(crate::environment::process_env_get(key))
        }
        "process_argv" => Some(crate::environment::process_argv()),
        "process_cwd" => Some(crate::environment::process_cwd()),
        _ => None,
    }
}

macro_rules! sync_dispatchers {
    ($($name:ident => [$($dispatcher:ident),+];)+) => {
        $(
            #[no_mangle]
            pub extern "C" fn $name(func_name_id: f64, arg_count: f64, base_addr: i32) -> f64 {
                mem_call_sync(func_name_id, arg_count, base_addr, |name, args| {
                    None$(.or_else(|| $dispatcher(name, args)))+
                })
            }
        )+
    };
}

sync_dispatchers! {
    mem_call_clocks => [dispatch_clocks];
    mem_call_random => [dispatch_random];
    mem_call_env => [dispatch_env];
    mem_call_fs => [dispatch_filesystem];
    mem_call_clocks_random => [dispatch_clocks, dispatch_random];
    mem_call_clocks_env => [dispatch_clocks, dispatch_env];
    mem_call_clocks_fs => [dispatch_clocks, dispatch_filesystem];
    mem_call_random_env => [dispatch_random, dispatch_env];
    mem_call_random_fs => [dispatch_random, dispatch_filesystem];
    mem_call_env_fs => [dispatch_env, dispatch_filesystem];
    mem_call_clocks_random_env => [dispatch_clocks, dispatch_random, dispatch_env];
    mem_call_clocks_random_fs => [dispatch_clocks, dispatch_random, dispatch_filesystem];
    mem_call_clocks_env_fs => [dispatch_clocks, dispatch_env, dispatch_filesystem];
    mem_call_random_env_fs => [dispatch_random, dispatch_env, dispatch_filesystem];
    mem_call_all_sync => [dispatch_clocks, dispatch_random, dispatch_env, dispatch_filesystem];
}

/// Monomorphization leaves only each entrypoint's selected capabilities reachable.
fn mem_call_sync(
    func_name_id: f64,
    arg_count: f64,
    base_addr: i32,
    dispatch: impl FnOnce(&str, &[i64]) -> Option<i64>,
) -> f64 {
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

    if let Some(result) = dispatch(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = result;
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

    if let Some(res) = dispatch_env(&name, &raw_args) {
        unsafe {
            *(base_addr as *mut i64) = res;
        }
        return 0.0;
    }

    if let Some(res) = dispatch_filesystem(&name, &raw_args) {
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
                    Some(JsHandle::Response { id, .. }) => Some(*id),
                    _ => None,
                })
                .collect();
            crate::http::wait_for_responses(crate::http::get_responses(), &response_ids)
                .unwrap_or_else(|error| fail_with_error(&error));
        }
    } else if name == "await_promise" {
        if let Some(JsHandle::Response { id, .. }) =
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
        if let Some(JsHandle::Response { id, headers }) = state.get_handle(target_handle) {
            let id = *id;
            let cached_headers = *headers;
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
                "statusText" => state.alloc_string(""),
                "url" => {
                    let metadata = responses[id]
                        .metadata()
                        .unwrap_or_else(|error| fail_with_error(&error));
                    state.alloc_string(metadata.url.split('#').next().unwrap_or_default())
                }
                "headers" => {
                    if let Some(headers) = cached_headers {
                        headers
                    } else {
                        let metadata = responses[id]
                            .metadata()
                            .unwrap_or_else(|error| fail_with_error(&error));
                        let headers =
                            nanbox_pointer(state.alloc_handle(JsHandle::Headers(metadata.headers)));
                        if let Some(JsHandle::Response {
                            headers: cached, ..
                        }) = state.get_handle_mut(target_handle)
                        {
                            *cached = Some(headers);
                        }
                        headers
                    }
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
                    let h_id = state.alloc_handle(JsHandle::Response { id, headers: None });
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
            Some(JsHandle::Response { id, .. }) => Some(*id),
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
            Some(JsHandle::Response { id, .. }) => Some(*id),
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
    } else if name == "array_includes" || name == "includes" {
        if raw_args.len() >= 2 {
            return if crate::stubs::array_includes(raw_args[0], raw_args[1]) != 0
                || crate::stubs::string_includes(raw_args[0], raw_args[1]) != 0
            {
                1
            } else {
                0
            };
        }
    } else if name == "string_includes" {
        if raw_args.len() >= 2 {
            return crate::stubs::string_includes(raw_args[0], raw_args[1]);
        }
    } else if name == "string_startsWith" || name == "string_starts_with" {
        if raw_args.len() >= 2 {
            return crate::stubs::string_startsWith(raw_args[0], raw_args[1]);
        }
    } else if name == "string_endsWith" || name == "string_ends_with" {
        if raw_args.len() >= 2 {
            return crate::stubs::string_endsWith(raw_args[0], raw_args[1]);
        }
    } else if name == "object_has_property" {
        if raw_args.len() >= 2 {
            return crate::stubs::object_has_property(raw_args[0], raw_args[1]);
        }
    } else if name == "array_is_array" {
        if let Some(&arg) = raw_args.first() {
            if let Some(h) = state.get_handle(arg) {
                return match h {
                    JsHandle::Array(_) => 1,
                    JsHandle::Json(serde_json::Value::Array(_)) => 1,
                    _ => 0,
                };
            }
        }
    }

    0
}
