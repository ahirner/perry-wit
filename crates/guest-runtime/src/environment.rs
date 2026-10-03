//! WASI Preview 2 Environment implementation for process.env, process.argv, and process.cwd().

use crate::nanbox::{nanbox_pointer, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

/// Implements `process.env`: returns a JS Object containing host environment variables.
/// Lazily populated from `wasi:cli/environment::get-environment()`.
pub(crate) fn process_env() -> i64 {
    let state = get_state();
    if let Some(handle) = state.process_env {
        return handle;
    }

    let env_list = crate::bindings::wasi::cli::environment::get_environment();
    let mut map = serde_json::Map::with_capacity(env_list.len());
    for (k, v) in env_list {
        map.insert(k, serde_json::Value::String(v));
    }

    let handle = state.from_js_value(serde_json::Value::Object(map));
    state.process_env = Some(handle);
    handle
}

/// Implements `process.env[key]` / `process.env.KEY`:
/// Returns the string value for the key if defined, or undefined.
pub(crate) fn process_env_get(key_val: i64) -> i64 {
    let state = get_state();
    let key = state.get_string(key_val);

    let env_handle = if let Some(h) = state.process_env {
        h
    } else {
        process_env()
    };

    let state = get_state();
    if let Some(JsHandle::Object(properties)) = state.get_handle(env_handle) {
        properties.get(&key).unwrap_or(TAG_UNDEFINED as i64)
    } else {
        TAG_UNDEFINED as i64
    }
}

/// Implements `process.argv`: returns a JS Array of strings from `wasi:cli/environment::get-arguments()`.
pub(crate) fn process_argv() -> i64 {
    let state = get_state();
    if let Some(handle) = state.process_argv {
        return handle;
    }

    let args = crate::bindings::wasi::cli::environment::get_arguments();
    let mut array_items = Vec::with_capacity(args.len());
    for arg in args {
        let s_val = state.alloc_string(&arg);
        array_items.push(s_val);
    }

    let handle_id = state.alloc_handle(JsHandle::Array(array_items));
    let handle = nanbox_pointer(handle_id);
    state.process_argv = Some(handle);
    handle
}

/// Implements `process.cwd()`: returns initial working directory from `wasi:cli/environment::initial-cwd()`.
pub(crate) fn process_cwd() -> i64 {
    let state = get_state();
    let cwd_opt = crate::bindings::wasi::cli::environment::initial_cwd();
    let cwd = cwd_opt.unwrap_or_else(|| "/".to_string());
    state.alloc_string(&cwd)
}
