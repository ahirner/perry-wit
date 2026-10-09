//! Bodies, and constructed `Response` and `Request` objects.
use crate::{
    headers::set_default,
    readable::{Readable, decode, drain, snapshot_stream},
    *,
};

/// The body both `Response` and `Request` start with: a snapshot taken at construction, or a
/// stream (fetched responses). `body` creates the snapshot's stream on first access.
#[repr(C)]
pub(crate) struct Body {
    /// 1 once read.
    used: u32,
    /// The snapshot, or null without one.
    data: *mut u8,
    len: u32,
    /// The body's stream, once created.
    pub(crate) stream: *mut Readable,
}

impl Body {
    fn snapshot(view: *mut u8) -> Body {
        if view.is_null() {
            return Body::none();
        }
        // SAFETY: compiled code passes a `Uint8Array`.
        let source = unsafe { view_bytes(view) };
        let mut copy = Buffer::new();
        copy.extend_from_slice(source);
        let data = copy.as_slice().as_ptr() as *mut u8;
        Body {
            used: 0,
            data: if source.is_empty() { alloc(0) } else { data },
            len: source.len() as u32,
            stream: core::ptr::null_mut(),
        }
    }

    pub(crate) fn none() -> Body {
        Body {
            used: 0,
            data: core::ptr::null_mut(),
            len: 0,
            stream: core::ptr::null_mut(),
        }
    }

    /// A body read from `stream`.
    pub(crate) fn streamed(stream: *mut Readable) -> Body {
        Body {
            stream,
            ..Body::none()
        }
    }
}

fn body<'a>(object: *mut u8) -> &'a mut Body {
    // SAFETY: responses and requests start with their body.
    unsafe { &mut *(object as *mut Body) }
}

/// `bodyUsed` of a response or request.
#[unsafe(no_mangle)]
pub extern "C" fn web_body_used(object: *mut u8) -> u32 {
    let body = body(object);
    // SAFETY: bodies keep their streams.
    body.used | u32::from(!body.stream.is_null() && unsafe { (*body.stream).disturbed } != 0)
}

/// `body` of a response or request: its stream, or null without a body.
#[unsafe(no_mangle)]
pub extern "C" fn web_body_stream(object: *mut u8) -> *mut u8 {
    let body = body(object);
    if body.stream.is_null() && !body.data.is_null() && body.used == 0 {
        body.stream = snapshot_stream(body.data, body.len);
    }
    body.stream as *mut u8
}

/// Take the body for reading: its bytes, or a rejected promise if it was read already.
fn consume(object: *mut u8, promise: *mut u8) -> Option<&'static [u8]> {
    let body = body(object);
    if body.used != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Body is unusable: Body has already been read"),
        );
        return None;
    }
    if body.data.is_null() {
        return Some(&[]);
    }
    body.used = 1;
    // SAFETY: the snapshot lives in the task region.
    Some(unsafe { slice(body.data as u32, body.len) })
}

/// `await body.bytes()`: a fresh `Uint8Array` of the body.
#[unsafe(no_mangle)]
pub extern "C" fn web_body_bytes(object: *mut u8) -> *mut u8 {
    let promise = promise_new();
    if !body(object).stream.is_null() {
        drain(body(object).stream, false, promise);
    } else if let Some(contents) = consume(object, promise) {
        let mut copy = Buffer::new();
        copy.extend_from_slice(contents);
        let view = bytes_view(contents.len() as u32, copy.as_slice().as_ptr() as *mut u8);
        resolve(promise, view as usize as u64);
    }
    promise
}

/// `await body.text()`: the body decoded as UTF-8 (BOM removed, invalid bytes replaced).
#[unsafe(no_mangle)]
pub extern "C" fn web_body_text(object: *mut u8) -> *mut u8 {
    let promise = promise_new();
    if !body(object).stream.is_null() {
        drain(body(object).stream, true, promise);
    } else if let Some(contents) = consume(object, promise) {
        resolve(promise, decode(contents) as usize as u64);
    }
    promise
}

// --- Response -----------------------------------------------------------------------------

#[repr(C)]
pub(crate) struct Response {
    pub(crate) body: Body,
    pub(crate) status: f64,
    pub(crate) status_text: *mut u8,
    pub(crate) headers: *mut u8,
}

fn response<'a>(r: *mut u8) -> &'a Response {
    // SAFETY: compiled code passes responses.
    unsafe { &*(r as *const Response) }
}

/// `new Response(body, { status, statusText, headers })`; `text` marks a string body.
///
/// # Safety
/// `view` must be null or a `Uint8Array`, `status_text` a string and `h` a header list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn web_response_new(
    view: *mut u8,
    text: u32,
    status: f64,
    status_text: *mut u8,
    h: *mut u8,
) -> *mut u8 {
    if !(200.0..=599.0).contains(&status) || status != (status as u32) as f64 {
        perry_abi::raise(
            RANGE_ERROR,
            "init[\"status\"] must be in the range of 200 to 599, inclusive.",
        );
        return core::ptr::null_mut();
    }
    // SAFETY: compiled code passes a string.
    let reason = unsafe { bytes(status_text) };
    // SAFETY: helper call on an input string.
    if unsafe { helper::fetch_status_text(reason.as_ptr(), reason.len() as u32) } == INVALID {
        perry_abi::raise(TYPE_ERROR, "Invalid statusText");
        return core::ptr::null_mut();
    }
    if !view.is_null() && matches!(status as u32, 204 | 205 | 304) {
        perry_abi::raise(
            TYPE_ERROR,
            "Response constructor: Invalid response status code",
        );
        return core::ptr::null_mut();
    }
    if text != 0 {
        set_default(h, b"content-type", b"text/plain;charset=UTF-8");
    }
    let r = alloc(core::mem::size_of::<Response>() as u32) as *mut Response;
    // SAFETY: a fresh block.
    unsafe {
        r.write(Response {
            body: Body::snapshot(view),
            status,
            status_text,
            headers: h,
        })
    };
    r as *mut u8
}

/// `response.status`.
#[unsafe(no_mangle)]
pub extern "C" fn web_response_status(r: *mut u8) -> f64 {
    response(r).status
}

/// `response.statusText`.
#[unsafe(no_mangle)]
pub extern "C" fn web_response_status_text(r: *mut u8) -> *mut u8 {
    response(r).status_text
}

/// `response.ok`.
#[unsafe(no_mangle)]
pub extern "C" fn web_response_ok(r: *mut u8) -> u32 {
    u32::from((200.0..300.0).contains(&response(r).status))
}

/// `response.headers`.
#[unsafe(no_mangle)]
pub extern "C" fn web_response_headers(r: *mut u8) -> *mut u8 {
    response(r).headers
}

// --- Request ------------------------------------------------------------------------------

#[repr(C)]
struct Request {
    body: Body,
    url: *mut u8,
    method: *mut u8,
    headers: *mut u8,
}

fn request<'a>(r: *mut u8) -> &'a Request {
    // SAFETY: compiled code passes requests.
    unsafe { &*(r as *const Request) }
}

/// Methods the fetch helper normalizes, by its tag.
const METHODS: [&str; 7] = ["GET", "HEAD", "POST", "PUT", "DELETE", "", "OPTIONS"];

/// `new Request(url, { method, headers, body })`; `text` marks a string body.
///
/// # Safety
/// `url` and `method` must be strings, `view` null or a `Uint8Array`, `h` a header list.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn web_request_new(
    url: *mut u8,
    method: *mut u8,
    view: *mut u8,
    text: u32,
    h: *mut u8,
) -> *mut u8 {
    // SAFETY: compiled code passes strings.
    let input = unsafe { bytes(url) };
    let capacity = 32 + input.len() as u32 * 3 + 16;
    let output = alloc(capacity);
    // SAFETY: helper call with an input string and a fresh disjoint output.
    let status = unsafe { helper::fetch_url(input.as_ptr(), input.len() as u32, output, capacity) };
    if status != 0 {
        type_error(&[b"Failed to parse URL from ", input]);
        return core::ptr::null_mut();
    }
    // SAFETY: the helper wrote six little-endian lengths, then the URL at 32.
    let normalized = unsafe {
        let len = *(output.add(20) as *const u32);
        from_bytes(slice(output as u32 + 32, len))
    };
    // SAFETY: compiled code passes strings.
    let name = unsafe { bytes(method) };
    // SAFETY: helper call on an input string.
    let tag = unsafe { helper::fetch_method(name.as_ptr(), name.len() as u32) };
    let method = match tag {
        INVALID => {
            type_error(&[b"'", name, b"' is not a valid HTTP method."]);
            return core::ptr::null_mut();
        }
        tag if (tag as usize) < METHODS.len() && !METHODS[tag as usize].is_empty() => {
            from_static(METHODS[tag as usize])
        }
        _ => method,
    };
    if !view.is_null() && matches!(tag, 0 | 1) {
        perry_abi::raise(TYPE_ERROR, "Request with GET/HEAD method cannot have body.");
        return core::ptr::null_mut();
    }
    if text != 0 {
        set_default(h, b"content-type", b"text/plain;charset=UTF-8");
    }
    let r = alloc(core::mem::size_of::<Request>() as u32) as *mut Request;
    // SAFETY: a fresh block.
    unsafe {
        r.write(Request {
            body: Body::snapshot(view),
            url: normalized,
            method,
            headers: h,
        })
    };
    r as *mut u8
}

/// `request.method`.
#[unsafe(no_mangle)]
pub extern "C" fn web_request_method(r: *mut u8) -> *mut u8 {
    request(r).method
}

/// `request.url`.
#[unsafe(no_mangle)]
pub extern "C" fn web_request_url(r: *mut u8) -> *mut u8 {
    request(r).url
}

/// `request.headers`.
#[unsafe(no_mangle)]
pub extern "C" fn web_request_headers(r: *mut u8) -> *mut u8 {
    request(r).headers
}
