//! Supported fetch options, validated before dispatching a request.

use serde_json::Value;

pub(crate) enum RequestMethod {
    Get,
    Post,
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
    for (key, value) in map {
        match key.as_str() {
            "method" => {
                options.method = match value.as_str() {
                    Some(method) if method.eq_ignore_ascii_case("GET") => RequestMethod::Get,
                    Some(method) if method.eq_ignore_ascii_case("POST") => RequestMethod::Post,
                    _ => return Err("fetch supports only GET and POST methods".into()),
                };
            }
            "body" => {
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
    if matches!(options.method, RequestMethod::Get) && !options.body.is_empty() {
        return Err("GET requests cannot have a body".into());
    }
    Ok(options)
}
