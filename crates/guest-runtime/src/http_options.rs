//! Supported fetch options, validated before dispatching a request.

use crate::nanbox::{STRING_TAG, TAG_NULL, TAG_UNDEFINED};
use crate::state::{JsHandle, RuntimeState};

pub(crate) enum RequestMethod {
    Get,
    Post,
    Head,
    Put,
    Patch,
    Delete,
    Options,
    Other(String),
}

pub(crate) struct RequestOptions {
    pub(crate) method: RequestMethod,
    pub(crate) headers: Vec<(String, Vec<u8>)>,
    pub(crate) body: Vec<u8>,
}

pub(crate) fn parse_options(state: &RuntimeState, value: i64) -> Result<RequestOptions, String> {
    let mut options = RequestOptions {
        method: RequestMethod::Get,
        headers: Vec::new(),
        body: Vec::new(),
    };
    if matches!(value as u64, TAG_NULL | TAG_UNDEFINED) {
        return Ok(options);
    }
    let Some(JsHandle::Object(properties)) = state.get_handle(value) else {
        return Err("fetch options must be an object".into());
    };
    let mut has_body = false;
    for (key, value) in properties.entries() {
        let value = *value;
        if value as u64 == TAG_UNDEFINED {
            continue;
        }
        match key.as_str() {
            "method" => {
                if (value as u64) >> 48 != STRING_TAG {
                    return Err("Invalid fetch method".into());
                }
                let method = state.get_string(value);
                options.method = match method.as_str() {
                    method if method.eq_ignore_ascii_case("GET") => RequestMethod::Get,
                    method if method.eq_ignore_ascii_case("POST") => RequestMethod::Post,
                    method if method.eq_ignore_ascii_case("HEAD") => RequestMethod::Head,
                    method if method.eq_ignore_ascii_case("PUT") => RequestMethod::Put,
                    "PATCH" => RequestMethod::Patch,
                    method if method.eq_ignore_ascii_case("DELETE") => RequestMethod::Delete,
                    method if method.eq_ignore_ascii_case("OPTIONS") => RequestMethod::Options,
                    method
                        if ["CONNECT", "TRACE", "TRACK"]
                            .iter()
                            .any(|forbidden| method.eq_ignore_ascii_case(forbidden)) =>
                    {
                        return Err("Forbidden fetch method".into());
                    }
                    method if valid_token(method) => RequestMethod::Other(method.into()),
                    _ => return Err("Invalid fetch method".into()),
                };
            }
            "body" => {
                has_body = value as u64 != TAG_NULL;
                options.body = if value as u64 == TAG_NULL {
                    Vec::new()
                } else if (value as u64) >> 48 == STRING_TAG {
                    state.get_string(value).into_bytes()
                } else if let Some(JsHandle::Uint8Array(view)) = state.get_handle(value) {
                    view.to_vec()
                } else {
                    return Err("fetch body must be a string or Uint8Array".into());
                };
            }
            "headers" => {
                options.headers = parse_headers(state, value)?;
            }
            _ => return Err(format!("Unsupported fetch option: {key}")),
        }
    }
    if matches!(options.method, RequestMethod::Get | RequestMethod::Head) && has_body {
        return Err("GET and HEAD requests cannot have a body".into());
    }
    Ok(options)
}

/// Extracts shared header inputs without creating resources or mutating guest values.
pub(crate) fn parse_headers(
    state: &RuntimeState,
    value: i64,
) -> Result<Vec<(String, Vec<u8>)>, String> {
    if let Some(JsHandle::Headers(headers)) = state.get_handle(value) {
        return Ok(headers
            .iter()
            .map(|(name, value)| (name.clone(), value.as_bytes().to_vec()))
            .collect());
    }
    let mut headers = Vec::new();
    let entries: Vec<_> = match state.get_handle(value) {
        Some(JsHandle::Object(headers)) => headers
            .entries()
            .map(|(key, value)| (key.clone(), *value))
            .collect(),
        Some(JsHandle::Array(headers)) => headers
            .iter()
            .map(|&header| match state.get_handle(header) {
                Some(JsHandle::Array(pair)) if pair.len() == 2 => {
                    if (pair[0] as u64) >> 48 != STRING_TAG {
                        return Err("fetch header names must be strings");
                    }
                    Ok((state.get_string(pair[0]), pair[1]))
                }
                _ => Err("fetch headers must contain name/value pairs"),
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ if value as u64 == TAG_NULL => Vec::new(),
        _ => return Err("fetch headers must be an object or name/value pairs".into()),
    };
    for (name, value) in entries {
        if (value as u64) >> 48 != STRING_TAG {
            return Err("fetch header values must be strings".into());
        }
        headers.push((name, state.get_string(value).into_bytes()));
    }
    Ok(headers)
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}
