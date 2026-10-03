//! Read-only response Headers values without host resource dependencies.

use crate::io::fail_with_error;
use crate::nanbox::{TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::state::{get_state, JsHandle};

pub(crate) fn dispatch_call(name: &str, args: &[i64]) -> Option<i64> {
    let state = get_state();
    let JsHandle::Headers(headers) = state.get_handle(*args.first()?)? else {
        return None;
    };
    let headers = headers.clone();
    let (method, arguments) = if name == "class_call_method" {
        let method = state.get_string(*args.get(1)?);
        let Some(JsHandle::Array(arguments)) =
            args.get(2).and_then(|&value| state.get_handle(value))
        else {
            fail_with_error("Invalid Headers method arguments");
        };
        (method, arguments.clone())
    } else if matches!(
        name,
        "get"
            | "has"
            | "set"
            | "append"
            | "delete"
            | "entries"
            | "keys"
            | "values"
            | "forEach"
            | "getSetCookie"
    ) {
        (name.into(), args[1..].to_vec())
    } else {
        return None;
    };
    if !matches!(method.as_str(), "get" | "has") {
        fail_with_error(&format!("Unsupported Headers method: {method}"));
    }
    let name = state.get_string(arguments.first().copied().unwrap_or(TAG_UNDEFINED as i64));
    if !crate::http_options::valid_token(&name) {
        fail_with_error("Invalid HTTP header name");
    }
    let values: Vec<_> = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(&name))
        .map(|(_, value)| value.trim_matches([' ', '\t']))
        .collect();
    Some(if method == "has" {
        (if values.is_empty() {
            TAG_FALSE
        } else {
            TAG_TRUE
        }) as i64
    } else if values.is_empty() {
        TAG_NULL as i64
    } else {
        state.alloc_string(&values.join(", "))
    })
}
