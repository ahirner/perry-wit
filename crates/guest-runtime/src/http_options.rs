//! Supported fetch options, validated before dispatching a request.

use serde_json::Value;

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

pub(crate) fn parse_options(value: Value) -> Result<RequestOptions, String> {
    let mut options = RequestOptions {
        method: RequestMethod::Get,
        headers: Vec::new(),
        body: Vec::new(),
    };
    let map = match value {
        Value::Null => return Ok(options),
        Value::Object(map) => map,
        _ => return Err("fetch options must be an object".into()),
    };
    let mut has_body = false;
    for (key, value) in map {
        match key.as_str() {
            "method" => {
                options.method = match value.as_str() {
                    Some(method) if method.eq_ignore_ascii_case("GET") => RequestMethod::Get,
                    Some(method) if method.eq_ignore_ascii_case("POST") => RequestMethod::Post,
                    Some(method) if method.eq_ignore_ascii_case("HEAD") => RequestMethod::Head,
                    Some(method) if method.eq_ignore_ascii_case("PUT") => RequestMethod::Put,
                    Some("PATCH") => RequestMethod::Patch,
                    Some(method) if method.eq_ignore_ascii_case("DELETE") => RequestMethod::Delete,
                    Some(method) if method.eq_ignore_ascii_case("OPTIONS") => {
                        RequestMethod::Options
                    }
                    Some(method)
                        if ["CONNECT", "TRACE", "TRACK"]
                            .iter()
                            .any(|forbidden| method.eq_ignore_ascii_case(forbidden)) =>
                    {
                        return Err("Forbidden fetch method".into())
                    }
                    Some(method) if valid_token(method) => RequestMethod::Other(method.into()),
                    _ => return Err("Invalid fetch method".into()),
                };
            }
            "body" => {
                has_body = !value.is_null();
                options.body = match value {
                    Value::Null => Vec::new(),
                    Value::String(body) => body.into_bytes(),
                    _ => return Err("fetch body must be a string".into()),
                };
            }
            "headers" => {
                let entries = match value {
                    Value::Null => Vec::new(),
                    Value::Object(headers) => headers.into_iter().collect(),
                    Value::Array(headers) => headers
                        .into_iter()
                        .map(|header| match header {
                            Value::Array(mut pair) if pair.len() == 2 => {
                                let value = pair.pop().unwrap();
                                match pair.pop().unwrap() {
                                    Value::String(name) => Ok((name, value)),
                                    _ => Err("fetch header names must be strings"),
                                }
                            }
                            _ => Err("fetch headers must contain name/value pairs"),
                        })
                        .collect::<Result<Vec<_>, _>>()?,
                    _ => return Err("fetch headers must be an object or name/value pairs".into()),
                };
                for (name, value) in entries {
                    let Value::String(value) = value else {
                        return Err("fetch header values must be strings".into());
                    };
                    options.headers.push((name, value.into_bytes()));
                }
            }
            _ => return Err(format!("Unsupported fetch option: {key}")),
        }
    }
    if matches!(options.method, RequestMethod::Get | RequestMethod::Head) && has_body {
        return Err("GET and HEAD requests cannot have a body".into());
    }
    Ok(options)
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}
