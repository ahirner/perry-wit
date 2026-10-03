//! Incoming HTTP metadata and body ownership through generated WASI bindings.

use std::cell::RefCell;

use crate::bindings::exports::wasi::http::incoming_handler::Guest;
use crate::bindings::wasi::http::types::{
    ErrorCode, Fields, IncomingBody, IncomingRequest, Method, OutgoingBody, OutgoingResponse,
    ResponseOutparam, Scheme,
};
use crate::bindings::wasi::io::poll::{poll, Pollable};
use crate::bindings::wasi::io::streams::{InputStream, StreamError};
use crate::nanbox::{nanbox_pointer, STRING_TAG, TAG_FALSE, TAG_NULL, TAG_TRUE, TAG_UNDEFINED};
use crate::objects::ObjectProperties;
use crate::state::{get_state, JsHandle};

pub(crate) const BODY_LIMIT: usize = 1_048_576;

/// A stream reference remains pure; host resources belong to the incoming invocation.
#[derive(Clone, Debug)]
pub(crate) enum Body {
    Bytes(Vec<u8>),
    Stream(i64),
}

/// A consumed source cannot alias a recycled resource slot or be forwarded twice.
#[derive(Clone, Debug)]
pub(crate) enum ReadableBody {
    Incoming(usize),
    Bytes(Vec<u8>),
    Consumed,
}

/// Metadata can escape; unread stream resources close at the invocation boundary.
#[derive(Clone, Debug)]
pub(crate) enum HttpMessage {
    Request {
        properties: ObjectProperties,
        body: Body,
    },
    Response {
        properties: ObjectProperties,
        body: Body,
    },
}

impl HttpMessage {
    pub(crate) fn properties(&self) -> &ObjectProperties {
        match self {
            Self::Request { properties, .. } | Self::Response { properties, .. } => properties,
        }
    }

    fn body(&self) -> &Body {
        match self {
            Self::Request { body, .. } | Self::Response { body, .. } => body,
        }
    }

    pub(crate) fn trace(&self, roots: &mut Vec<i64>) {
        roots.extend(self.properties().entries().map(|(_, value)| *value));
        if let Body::Stream(stream) = self.body() {
            roots.push(*stream);
        }
    }
}

/// Declaration order releases stream children before body and request parents.
struct IncomingInput {
    stream: InputStream,
    _body: IncomingBody,
    _request: IncomingRequest,
}

/// Resource storage is separate from JS handles so post-return never drops host resources.
#[derive(Default)]
struct IncomingInputs {
    sources: Vec<Option<IncomingInput>>,
    waiting: Option<Pollable>,
}

thread_local! {static INPUTS: RefCell<IncomingInputs> = RefCell::default();}

/// Owns output resources and the guest value rooted during callback progress.
struct PreparedResponse {
    body: OutgoingBody,
    response: OutgoingResponse,
    payload: Body,
    root: i64,
}

/// Generated bindings own the request/outparam boundary; hooks own guest execution.
struct Handler;

impl Guest for Handler {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        cancel_inputs();
        let response = if cabi_http_handler_begin() == 0 {
            drop(request);
            Err(guest_error())
        } else {
            read_request(request).and_then(|value| {
                let result = cabi_http_handler_invoke(value);
                if get_state().current_exception.is_some() {
                    Err(guest_error())
                } else {
                    prepare_response(result)
                }
            })
        };
        match response {
            Ok(prepared) => {
                if matches!(prepared.payload, Body::Bytes(_)) {
                    cabi_http_handler_drain(prepared.root);
                }
                if get_state().current_exception.is_some() {
                    drop(prepared);
                    ResponseOutparam::set(response_out, Err(guest_error()));
                } else {
                    let PreparedResponse {
                        body,
                        response,
                        payload,
                        root,
                    } = prepared;
                    ResponseOutparam::set(response_out, Ok(response));
                    if send_body(body, payload, root) {
                        cabi_http_handler_drain(root);
                    } else {
                        get_state().timers.cancel_all();
                        get_state().promises.cancel_all();
                    }
                }
            }
            Err(error) => ResponseOutparam::set(response_out, Err(error)),
        }
        get_state().timers.cancel_all();
        get_state().promises.cancel_all();
        cancel_inputs();
        cabi_http_handler_end();
    }
}

crate::bindings::export!(Handler with_types_in crate::bindings);

/// Owns body transmission and drops its stream before finishing or closing the body.
fn send_body(body: OutgoingBody, payload: Body, root: i64) -> bool {
    let Ok(output) = body.write() else {
        return false;
    };
    let sent = match payload {
        Body::Bytes(bytes) => {
            crate::streams::write(&output, &bytes, |pollable| wait_stream(pollable, root))
        }
        Body::Stream(handle) => {
            if let Some(bytes) = take_bytes(handle) {
                crate::streams::write(&output, &bytes, |pollable| wait_stream(pollable, root))
            } else {
                take_input(handle).and_then(|input| {
                    crate::streams::forward(&input.stream, &output, |pollable| {
                        wait_stream(pollable, root)
                    })
                })
            }
        }
    };
    drop(output);
    if sent.is_err() {
        return false;
    }
    OutgoingBody::finish(body, None).is_ok()
}

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

/// The ABI replaces this with one microtask/readiness step and a safe reclamation boundary.
#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn cabi_http_handler_step(root: i64) -> i32 {
    std::hint::black_box(root);
    std::hint::black_box(0)
}

/// Remaining jobs run after forwarding, or before publishing a buffered response.
#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn cabi_http_handler_drain(root: i64) {
    std::hint::black_box(root);
    std::hint::black_box(get_state());
}

fn guest_error() -> ErrorCode {
    ErrorCode::InternalError(Some(
        get_state()
            .current_exception
            .take()
            .unwrap_or_else(|| "HTTP handler could not execute".into()),
    ))
}

/// Creates metadata and an owned body source without buffering the request first.
fn read_request(request: IncomingRequest) -> Result<i64, ErrorCode> {
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
    let body = INPUTS.with(|inputs| {
        let mut inputs = inputs.borrow_mut();
        let index = inputs
            .sources
            .iter()
            .position(Option::is_none)
            .unwrap_or(inputs.sources.len());
        let source = Some(IncomingInput {
            stream,
            _body: incoming_body,
            _request: request,
        });
        if index == inputs.sources.len() {
            inputs.sources.push(source);
        } else {
            inputs.sources[index] = source;
        }
        nanbox_pointer(
            get_state().alloc_handle(JsHandle::ReadableBody(ReadableBody::Incoming(index))),
        )
    });
    let state = get_state();
    let headers = nanbox_pointer(state.alloc_handle(JsHandle::Headers(entries)));
    let mut properties = ObjectProperties::default();
    properties.insert("method".into(), state.alloc_string(&method));
    properties.insert("url".into(), state.alloc_string(&url));
    properties.insert("headers".into(), headers);
    properties.insert("body".into(), body);
    Ok(nanbox_pointer(state.alloc_handle(JsHandle::HttpMessage(
        HttpMessage::Request {
            properties,
            body: Body::Stream(body),
        },
    ))))
}

/// Consuming marks the shared JS stream value before removing its resource owner.
fn take_input(handle: i64) -> Result<IncomingInput, String> {
    let Some(JsHandle::ReadableBody(source @ ReadableBody::Incoming(_))) =
        get_state().get_handle_mut(handle)
    else {
        return Err("TypeError: Invalid incoming body stream".into());
    };
    let ReadableBody::Incoming(index) = std::mem::replace(source, ReadableBody::Consumed) else {
        unreachable!()
    };
    INPUTS
        .with(|inputs| {
            inputs
                .borrow_mut()
                .sources
                .get_mut(index)
                .and_then(Option::take)
        })
        .ok_or_else(|| "TypeError: Incoming body stream is no longer available".into())
}

/// Materialized methods retain the bounded, repeatable buffered-body subset.
fn read_buffered(handle: i64) -> Result<Vec<u8>, String> {
    let input = take_input(handle)?;
    let mut body = Vec::new();
    loop {
        match input.stream.blocking_read(8192) {
            Ok(chunk) => {
                if chunk.len() > BODY_LIMIT - body.len() {
                    return Err("RangeError: Request body exceeds the 1048576 byte limit".into());
                }
                body.extend_from_slice(&chunk);
            }
            Err(StreamError::Closed) => break,
            Err(error) => return Err(format!("Incoming body read failed: {error:?}")),
        }
    }
    Ok(body)
}

/// Acquires response resources only after validating the guest result.
fn prepare_response(value: i64) -> Result<PreparedResponse, ErrorCode> {
    let state = get_state();
    let Some(JsHandle::HttpMessage(HttpMessage::Response { properties, body })) =
        state.get_handle(value)
    else {
        return Err(ErrorCode::InternalError(Some(
            "Incoming handler must return a Response".into(),
        )));
    };
    let body = body.clone();
    if matches!(&body, Body::Stream(handle)
        if !matches!(state.get_handle(*handle), Some(JsHandle::ReadableBody(ReadableBody::Incoming(_) | ReadableBody::Bytes(_)))))
    {
        return Err(ErrorCode::InternalError(Some(
            "Response body stream has already been consumed".into(),
        )));
    }
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
    Ok(PreparedResponse {
        response,
        body: outgoing_body,
        payload: body,
        root: value,
    })
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
    if matches!(
        state.get_handle(*arguments.first()?),
        Some(JsHandle::ReadableBody(_))
    ) {
        if matches!(
            name,
            "class_call_method"
                | "getReader"
                | "cancel"
                | "pipeTo"
                | "pipeThrough"
                | "tee"
                | "values"
        ) {
            state.current_exception = Some("TypeError: Body streams currently support forwarding through Response; reader methods are not implemented".into());
            return Some(TAG_UNDEFINED as i64);
        }
        return None;
    }
    let JsHandle::HttpMessage(message) = state.get_handle(*arguments.first()?)? else {
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
    let body = match message.body() {
        Body::Bytes(bytes) => bytes.clone(),
        Body::Stream(handle) => {
            let handle = *handle;
            if matches!(
                state.get_handle(handle),
                Some(JsHandle::ReadableBody(ReadableBody::Consumed))
            ) {
                state.current_exception =
                    Some("TypeError: Body stream has already been consumed".into());
                return Some(TAG_UNDEFINED as i64);
            }
            let bytes = take_bytes(handle)?;
            if let Some(JsHandle::HttpMessage(message)) = state.get_handle_mut(*arguments.first()?)
            {
                match message {
                    HttpMessage::Request { body, .. } | HttpMessage::Response { body, .. } => {
                        *body = Body::Bytes(bytes.clone())
                    }
                }
            }
            bytes
        }
    };
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
        Some(JsHandle::Uint8Array(view)) => Body::Bytes(view.to_vec()),
        Some(JsHandle::ReadableBody(ReadableBody::Incoming(_) | ReadableBody::Bytes(_))) => {
            Body::Stream(body)
        }
        Some(JsHandle::ReadableBody(ReadableBody::Consumed)) => {
            return Err("TypeError: Response body stream has already been consumed".into())
        }
        _ if !supplied_body => Body::Bytes(Vec::new()),
        _ if (body as u64) >> 48 == STRING_TAG => Body::Bytes(state.get_string(body).into_bytes()),
        _ => return Err("TypeError: Response body must be a string or Uint8Array".into()),
    };
    if matches!(&body, Body::Bytes(bytes) if bytes.len() > BODY_LIMIT) {
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
    let body_handle = match &body {
        Body::Stream(handle) => *handle,
        Body::Bytes(bytes) if supplied_body => nanbox_pointer(
            state.alloc_handle(JsHandle::ReadableBody(ReadableBody::Bytes(bytes.clone()))),
        ),
        Body::Bytes(_) => TAG_NULL as i64,
    };
    properties.insert("body".into(), body_handle);
    properties.insert("status".into(), (status as f64).to_bits() as i64);
    properties.insert(
        "ok".into(),
        (if status < 300 { TAG_TRUE } else { TAG_FALSE }) as i64,
    );
    properties.insert("headers".into(), headers);
    properties.insert("statusText".into(), state.alloc_string(""));
    properties.insert("url".into(), state.alloc_string(""));
    Ok(nanbox_pointer(state.alloc_handle(JsHandle::HttpMessage(
        HttpMessage::Response { properties, body },
    ))))
}

/// Pure byte sources need no resource arena or host calls.
fn take_bytes(handle: i64) -> Option<Vec<u8>> {
    let Some(JsHandle::ReadableBody(source @ ReadableBody::Bytes(_))) =
        get_state().get_handle_mut(handle)
    else {
        return None;
    };
    let ReadableBody::Bytes(bytes) = std::mem::replace(source, ReadableBody::Consumed) else {
        unreachable!()
    };
    Some(bytes)
}

/// Incoming body methods use this bridge; unrelated calls retain their selected dispatcher.
#[no_mangle]
pub(crate) extern "C" fn mem_call_incoming(name: f64, count: f64, base: i32) -> f64 {
    crate::dispatch::try_mem_call(name, count, base, dispatch_incoming_call)
        .unwrap_or_else(|| guest_incoming_fallback(name, count, base))
}

/// The linker replaces this hook with the caller's capability-specific dispatcher.
#[no_mangle]
#[inline(never)]
pub(crate) extern "C" fn guest_incoming_fallback(name: f64, count: f64, base: i32) -> f64 {
    std::hint::black_box((name, count, base));
    std::hint::black_box(0.0)
}

fn dispatch_incoming_call(name: &str, arguments: &[i64]) -> Option<i64> {
    let target = *arguments.first()?;
    let state = get_state();
    let Some(JsHandle::HttpMessage(message)) = state.get_handle(target) else {
        return None;
    };
    let Body::Stream(stream) = message.body() else {
        return None;
    };
    let stream = *stream;
    if !matches!(
        state.get_handle(stream),
        Some(JsHandle::ReadableBody(ReadableBody::Incoming(_)))
    ) {
        return None;
    }
    let method = if name == "class_call_method" {
        state.get_string(*arguments.get(1)?)
    } else {
        name.into()
    };
    if !matches!(
        method.as_str(),
        "text" | "bytes" | "json" | "response_text" | "response_bytes" | "response_json"
    ) {
        return None;
    }
    match read_buffered(stream) {
        Ok(bytes) => {
            if let Some(JsHandle::HttpMessage(message)) = state.get_handle_mut(target) {
                match message {
                    HttpMessage::Request { body, .. } | HttpMessage::Response { body, .. } => {
                        *body = Body::Bytes(bytes)
                    }
                }
            }
            dispatch_call(name, arguments)
        }
        Err(error) => {
            state.current_exception = Some(error);
            Some(TAG_UNDEFINED as i64)
        }
    }
}

/// Releases host resources only from ordinary invocation code, not from value collection.
fn cancel_inputs() {
    INPUTS.with(|inputs| {
        let mut inputs = inputs.borrow_mut();
        inputs.waiting = None;
        for source in &mut inputs.sources {
            *source = None;
        }
    });
    for handle in &mut get_state().handles {
        if let JsHandle::ReadableBody(source @ ReadableBody::Incoming(_)) = handle {
            *source = ReadableBody::Consumed;
        }
    }
}

/// Runs guest work at a frame boundary while the pending I/O subscription remains owned.
fn wait_stream(pollable: Pollable, root: i64) -> Result<(), String> {
    INPUTS.with(|inputs| inputs.borrow_mut().waiting = Some(pollable));
    while cabi_http_handler_step(root) != 0 {}
    INPUTS.with(|inputs| inputs.borrow_mut().waiting = None);
    match get_state().current_exception.as_ref() {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    }
}

/// A timer-free handler never roots clock imports merely because it can forward a body.
#[no_mangle]
pub(crate) extern "C" fn guest_stream_step() -> i32 {
    INPUTS.with(|inputs| poll(&[inputs.borrow().waiting.as_ref().unwrap()]));
    0
}

/// Shared polling prevents a blocked body peer from starving the earliest guest timer.
#[no_mangle]
pub(crate) extern "C" fn guest_stream_step_timers() -> i32 {
    let timer_ready = INPUTS.with(|inputs| {
        let inputs = inputs.borrow();
        let stream = inputs.waiting.as_ref().unwrap();
        let state = get_state();
        if let Some(timer) = state.timers.next_pollable() {
            poll(&[stream, timer]).contains(&1)
        } else {
            poll(&[stream]);
            false
        }
    });
    if timer_ready {
        crate::timers::timers_step()
    } else {
        0
    }
}
