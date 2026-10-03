//! Buffered incoming requests and responses with generated WASI resource ownership.

use crate::bindings::exports::wasi::http::incoming_handler::Guest;
use crate::bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
    Scheme,
};
use crate::bindings::wasi::io::streams::StreamError;
use crate::nanbox::{nanbox_pointer, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::objects::ObjectProperties;
use crate::state::{get_state, JsHandle};

pub(crate) const BODY_LIMIT: usize = 1_048_576;

/// Retained values contain owned bytes and metadata, never borrowed host resources.
#[derive(Clone, Debug)]
pub(crate) enum BufferedMessage {
    Request {
        properties: ObjectProperties,
        body: Vec<u8>,
    },
    Response {
        properties: ObjectProperties,
        body: Vec<u8>,
    },
}

impl BufferedMessage {
    pub(crate) fn properties(&self) -> &ObjectProperties {
        match self {
            Self::Request { properties, .. } | Self::Response { properties, .. } => properties,
        }
    }

    fn body(&self) -> &[u8] {
        match self {
            Self::Request { body, .. } | Self::Response { body, .. } => body,
        }
    }
}

/// Generated bindings own the request/outparam boundary; hooks own guest execution.
struct Handler;

impl Guest for Handler {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let response = if cabi_http_handler_begin() == 0 {
            Err(guest_error())
        } else {
            read_request(&request).and_then(|value| {
                let result = cabi_http_handler_invoke(value);
                if get_state().current_exception.is_some() {
                    Err(guest_error())
                } else {
                    prepare_response(result)
                }
            })
        };
        drop(request);
        match response {
            Ok((response, body, bytes)) => {
                ResponseOutparam::set(response_out, Ok(response));
                if let Ok(stream) = body.write() {
                    let mut complete = true;
                    for chunk in bytes.chunks(4096) {
                        if stream.blocking_write_and_flush(chunk).is_err() {
                            complete = false;
                            break;
                        }
                    }
                    drop(stream);
                    if complete {
                        let _ = OutgoingBody::finish(body, None);
                    }
                }
            }
            Err(error) => ResponseOutparam::set(response_out, Err(error)),
        }
        get_state().timers.cancel_all();
        get_state().promises.cancel_all();
        cabi_http_handler_end();
    }
}

crate::bindings::export!(Handler with_types_in crate::bindings);

/// The compiler replaces these hooks with initialization, guest execution, and reclamation.
/// Black-box results keep LLVM from folding callers against these placeholder bodies.
#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn cabi_http_handler_begin() -> i32 {
    get_state().current_exception = Some("Incoming handler bridge is unavailable".into());
    std::hint::black_box(0)
}

#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn cabi_http_handler_invoke(request: i64) -> i64 {
    let request = std::hint::black_box(request);
    if std::hint::black_box(false) {
        request
    } else {
        get_state().current_exception = Some("Incoming handler bridge is unavailable".into());
        TAG_UNDEFINED as i64
    }
}

#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn cabi_http_handler_end() {
    get_state().current_exception = None;
}

#[no_mangle]
pub(crate) extern "C" fn cabi_http_handler_failed() -> i32 {
    i32::from(get_state().current_exception.is_some())
}

fn guest_error() -> ErrorCode {
    ErrorCode::InternalError(Some(
        get_state()
            .current_exception
            .take()
            .unwrap_or_else(|| "HTTP handler could not execute".into()),
    ))
}

/// Buffers before guest execution, releasing children before their owning request.
fn read_request(request: &IncomingRequest) -> Result<i64, ErrorCode> {
    let headers = request.headers();
    let entries: Vec<_> = headers
        .entries()
        .into_iter()
        .map(|(name, bytes)| {
            (
                name.to_ascii_lowercase(),
                bytes.into_iter().map(char::from).collect(),
            )
        })
        .collect();
    drop(headers);
    let method = match request.method() {
        Method::Get => "GET".into(),
        Method::Head => "HEAD".into(),
        Method::Post => "POST".into(),
        Method::Put => "PUT".into(),
        Method::Delete => "DELETE".into(),
        Method::Connect => "CONNECT".into(),
        Method::Options => "OPTIONS".into(),
        Method::Trace => "TRACE".into(),
        Method::Patch => "PATCH".into(),
        Method::Other(method) => method,
    };
    let scheme = match request.scheme() {
        Some(Scheme::Https) => "https".into(),
        Some(Scheme::Other(scheme)) => scheme,
        _ => "http".into(),
    };
    let url = format!(
        "{scheme}://{}{}",
        request.authority().unwrap_or_default(),
        request.path_with_query().unwrap_or_else(|| "/".into())
    );
    let incoming_body = request.consume().map_err(|_| {
        ErrorCode::InternalError(Some("Incoming request body already consumed".into()))
    })?;
    let stream = incoming_body.stream().map_err(|_| {
        ErrorCode::InternalError(Some("Incoming request stream unavailable".into()))
    })?;
    let mut body = Vec::new();
    loop {
        match stream.blocking_read(8192) {
            Ok(chunk) => {
                if chunk.len() > BODY_LIMIT - body.len() {
                    return Err(ErrorCode::HttpRequestBodySize(Some(BODY_LIMIT as u64)));
                }
                body.extend_from_slice(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(error) => {
                return Err(ErrorCode::InternalError(Some(format!(
                    "Incoming body read failed: {error:?}"
                ))))
            }
        }
    }
    drop(stream);
    drop(incoming_body);
    let state = get_state();
    let headers = nanbox_pointer(state.alloc_handle(JsHandle::Headers(entries)));
    let mut properties = ObjectProperties::default();
    properties.insert("method".into(), state.alloc_string(&method));
    properties.insert("url".into(), state.alloc_string(&url));
    properties.insert("headers".into(), headers);
    Ok(nanbox_pointer(state.alloc_handle(JsHandle::BufferedHttp(
        BufferedMessage::Request { properties, body },
    ))))
}

/// Acquires response resources only after validating the guest result.
fn prepare_response(value: i64) -> Result<(OutgoingResponse, OutgoingBody, Vec<u8>), ErrorCode> {
    let state = get_state();
    let Some(JsHandle::BufferedHttp(BufferedMessage::Response { properties, body })) =
        state.get_handle(value)
    else {
        return Err(ErrorCode::InternalError(Some(
            "Incoming handler must return a Response".into(),
        )));
    };
    let body = body.clone();
    let status = state.to_number(properties.get("status").unwrap()) as u16;
    let Some(JsHandle::Headers(headers)) = state.get_handle(properties.get("headers").unwrap())
    else {
        unreachable!()
    };
    let headers: Vec<_> = headers
        .iter()
        .map(|(name, value)| {
            (
                name.clone(),
                value.chars().map(|character| character as u8).collect(),
            )
        })
        .collect();
    let fields = Fields::from_list(&headers).map_err(|error| {
        ErrorCode::InternalError(Some(format!("Invalid response headers: {error:?}")))
    })?;
    let response = OutgoingResponse::new(fields);
    response
        .set_status_code(status)
        .map_err(|_| ErrorCode::InternalError(Some("Invalid response status".into())))?;
    let outgoing_body = response
        .body()
        .map_err(|_| ErrorCode::InternalError(Some("Outgoing response body unavailable".into())))?;
    Ok((response, outgoing_body, body))
}

/// Buffered value methods have no host-resource dependencies.
pub(crate) fn dispatch_call(name: &str, arguments: &[i64]) -> Option<i64> {
    if name == "http_response_new" {
        let arguments = if arguments.first() == Some(&(TAG_UNDEFINED as i64)) {
            &arguments[1..]
        } else {
            arguments
        };
        let value = construct_response(
            arguments.first().copied().unwrap_or(TAG_UNDEFINED as i64),
            arguments.get(1).copied().unwrap_or(TAG_UNDEFINED as i64),
        );
        return Some(match value {
            Ok(value) => value,
            Err(error) => {
                get_state().current_exception = Some(error);
                TAG_UNDEFINED as i64
            }
        });
    }
    let state = get_state();
    let JsHandle::BufferedHttp(message) = state.get_handle(*arguments.first()?)? else {
        return None;
    };
    let method = if name == "class_call_method" {
        state.get_string(*arguments.get(1)?)
    } else {
        name.into()
    };
    if !matches!(
        method.as_str(),
        "text" | "bytes" | "json" | "response_text" | "response_bytes" | "response_json"
    ) {
        if name == "class_call_method"
            || matches!(name, "arrayBuffer" | "blob" | "formData" | "clone")
        {
            state.current_exception = Some(format!(
                "TypeError: Unsupported buffered HTTP method: {method}"
            ));
            return Some(TAG_UNDEFINED as i64);
        }
        return None;
    }
    let body = message.body().to_vec();
    Some(match method.as_str() {
        "bytes" | "response_bytes" => nanbox_pointer(state.alloc_handle(JsHandle::Uint8Array(
            crate::buffer::Uint8ArrayView::from_bytes(body),
        ))),
        "json" | "response_json" => {
            match serde_json::from_str(&crate::http::decode_utf8_body(&body)) {
                Ok(value) => state.from_js_value(value),
                Err(error) => {
                    state.current_exception = Some(format!("SyntaxError: {error}"));
                    TAG_UNDEFINED as i64
                }
            }
        }
        _ => state.alloc_string(&crate::http::decode_utf8_body(&body)),
    })
}

/// Copies a bounded body and validates the supported options before making host calls.
fn construct_response(body: i64, options: i64) -> Result<i64, String> {
    let state = get_state();
    let supplied_body = !matches!(body as u64, TAG_NULL | TAG_UNDEFINED);
    let body = match state.get_handle(body) {
        Some(JsHandle::Uint8Array(view)) => view.to_vec(),
        _ if !supplied_body => Vec::new(),
        _ if (body as u64) >> 48 == STRING_TAG => state.get_string(body).into_bytes(),
        _ => return Err("TypeError: Response body must be a string or Uint8Array".into()),
    };
    if body.len() > BODY_LIMIT {
        return Err("RangeError: Response body exceeds the 1048576 byte limit".into());
    }
    let mut status = 200u16;
    let mut headers = Vec::new();
    if !matches!(options as u64, TAG_NULL | TAG_UNDEFINED) {
        let Some(JsHandle::Object(properties)) = state.get_handle(options) else {
            return Err("TypeError: Response options must be an object".into());
        };
        for (key, value) in properties.entries() {
            if *value as u64 == TAG_UNDEFINED {
                continue;
            }
            match key.as_str() {
                "status" => {
                    let number = state.to_number(*value).trunc();
                    if !number.is_finite() || !(200.0..=599.0).contains(&number) {
                        return Err("RangeError: Invalid Response status".into());
                    }
                    status = number as u16;
                }
                "headers" => {
                    headers = crate::http_options::parse_headers(state, *value)
                        .map_err(|error| format!("TypeError: {error}"))?
                }
                _ => return Err(format!("TypeError: Unsupported Response option: {key}")),
            }
        }
    }
    if supplied_body && matches!(status, 204 | 205 | 304) {
        return Err("TypeError: Response status cannot have a body".into());
    }
    let headers: Vec<_> = headers
        .into_iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), String::from_utf8(value).unwrap()))
        .collect();
    if headers.iter().any(|(name, value)| {
        !crate::http_options::valid_token(name)
            || value
                .chars()
                .any(|character| character as u32 > 255 || matches!(character, '\0' | '\n' | '\r'))
    }) {
        return Err("TypeError: Invalid Response header".into());
    }
    let headers = nanbox_pointer(
        state.alloc_handle(JsHandle::Headers(
            headers
                .into_iter()
                .map(|(name, value)| (name, value.trim_matches([' ', '\t']).into()))
                .collect(),
        )),
    );
    let mut properties = ObjectProperties::default();
    properties.insert("status".into(), (status as f64).to_bits() as i64);
    properties.insert(
        "ok".into(),
        (if status < 300 { TAG_TRUE } else { TAG_FALSE }) as i64,
    );
    properties.insert("headers".into(), headers);
    properties.insert("statusText".into(), state.alloc_string(""));
    properties.insert("url".into(), state.alloc_string(""));
    Ok(nanbox_pointer(state.alloc_handle(JsHandle::BufferedHttp(
        BufferedMessage::Response { properties, body },
    ))))
}
