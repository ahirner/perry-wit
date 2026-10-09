//! Fetch API values (`Headers`, constructed `Response` and `Request` objects with byte
//! bodies) and Web writers over the console outputs (`Writable.toWeb(process.stdout)`).
//!
//! A helper library like `perry-std`, linked only into programs that use these classes. Name,
//! value, method, status text and URL rules come from the fetch helper (`src/helpers/fetch`),
//! which the network `fetch` uses as well: header names are stored lowercased, values as
//! trimmed Latin-1 bytes (ByteStrings), and `get` joins duplicates with `, `.
//!
//! Bodies are snapshots taken at construction; reading one (`bytes()`, `text()`) marks it
//! used and resolves at once, and a second read rejects with `TypeError`, as in Node.
#![cfg_attr(target_arch = "wasm32", no_std)]

use perry_abi::{
    Buffer, RANGE_ERROR, TYPE_ERROR, alloc, bytes, bytes_view, from_bytes, from_static,
    promise_new, reject_with, resolve, view_bytes,
};

mod fetch {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "__perry_helper")]
    unsafe extern "C" {
        pub fn fetch_header_name(input: *const u8, len: u32, output: *mut u8, cap: u32) -> u32;
        pub fn fetch_header_value(input: *const u8, len: u32, output: *mut u8, cap: u32) -> u32;
        pub fn fetch_method(input: *const u8, len: u32) -> u32;
        pub fn fetch_status_text(input: *const u8, len: u32) -> u32;
        pub fn fetch_url(input: *const u8, len: u32, output: *mut u8, cap: u32) -> u32;
    }

    /// The fetch helper exists only in linked modules; host builds never call it.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    mod host {
        pub unsafe fn fetch_header_name(_: *const u8, _: u32, _: *mut u8, _: u32) -> u32 {
            unreachable!("the fetch helper runs in linked modules")
        }
        pub unsafe fn fetch_header_value(_: *const u8, _: u32, _: *mut u8, _: u32) -> u32 {
            unreachable!("the fetch helper runs in linked modules")
        }
        pub unsafe fn fetch_method(_: *const u8, _: u32) -> u32 {
            unreachable!("the fetch helper runs in linked modules")
        }
        pub unsafe fn fetch_status_text(_: *const u8, _: u32) -> u32 {
            unreachable!("the fetch helper runs in linked modules")
        }
        pub unsafe fn fetch_url(_: *const u8, _: u32, _: *mut u8, _: u32) -> u32 {
            unreachable!("the fetch helper runs in linked modules")
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub use host::*;
}

const INVALID: u32 = u32::MAX;

/// Throw `TypeError` with `parts` concatenated.
fn type_error(parts: &[&[u8]]) {
    let mut message = Buffer::new();
    for part in parts {
        message.extend_from_slice(part);
    }
    perry_abi::raise_string(TYPE_ERROR, from_bytes(message.as_slice()));
}

// --- Headers ------------------------------------------------------------------------------

/// A header list: 16-byte fields (`name`, `name_len`, `value`, `value_len`) in insertion
/// order, the layout the fetch helper reads.
#[repr(C)]
pub struct Headers {
    fields: Buffer,
}

fn headers<'a>(h: *mut u8) -> &'a mut Headers {
    // SAFETY: compiled code passes header lists.
    unsafe { &mut *(h as *mut Headers) }
}

impl Headers {
    fn fields(&self) -> impl Iterator<Item = (&'static [u8], &'static [u8])> + '_ {
        self.fields.as_slice().chunks_exact(16).map(|field| {
            let word = |at: usize| u32::from_le_bytes(field[at..at + 4].try_into().unwrap());
            // SAFETY: fields point at bytes in the task region.
            unsafe { (slice(word(0), word(4)), slice(word(8), word(12))) }
        })
    }

    fn push(&mut self, name: &[u8], value: &[u8]) {
        for word in [
            name.as_ptr() as u32,
            name.len() as u32,
            value.as_ptr() as u32,
            value.len() as u32,
        ] {
            self.fields.extend_from_slice(&word.to_le_bytes());
        }
    }

    /// Keep the fields `keep` accepts, in order.
    fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let mut kept = Buffer::new();
        for (field, (name, _)) in self.fields.as_slice().chunks_exact(16).zip(self.fields()) {
            if keep(name) {
                kept.extend_from_slice(field);
            }
        }
        self.fields = kept;
    }
}

/// # Safety
/// `data` must point to `len` bytes that outlive the use.
unsafe fn slice(data: u32, len: u32) -> &'static [u8] {
    if len == 0 {
        return &[];
    }
    // SAFETY: guaranteed by the caller.
    unsafe { core::slice::from_raw_parts(data as usize as *const u8, len as usize) }
}

/// A new empty header list.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_new() -> *mut u8 {
    let h = alloc(core::mem::size_of::<Headers>() as u32) as *mut Headers;
    // SAFETY: a fresh block.
    unsafe {
        h.write(Headers {
            fields: Buffer::new(),
        })
    };
    h as *mut u8
}

/// A copy of a header list (`new Headers(headers)`, constructor `headers` init).
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_copy(source: *mut u8) -> *mut u8 {
    let copy = web_headers_new();
    headers(copy)
        .fields
        .extend_from_slice(headers(source).fields.as_slice());
    copy
}

/// The lowercased name, or `TypeError` naming `operation`.
fn name(operation: &[u8], name: *mut u8) -> Option<&'static [u8]> {
    // SAFETY: compiled code passes strings.
    let text = unsafe { bytes(name) };
    let output = alloc(text.len() as u32);
    // SAFETY: helper call with an input string and a disjoint output of the same size.
    let len = unsafe {
        fetch::fetch_header_name(text.as_ptr(), text.len() as u32, output, text.len() as u32)
    };
    if len == INVALID {
        type_error(&[operation, b": \"", text, b"\" is an invalid header name."]);
        return None;
    }
    // SAFETY: the helper wrote `len` bytes.
    Some(unsafe { slice(output as u32, len) })
}

/// The trimmed Latin-1 value, or `TypeError` naming `operation`.
fn value(operation: &[u8], value: *mut u8) -> Option<&'static [u8]> {
    // SAFETY: compiled code passes strings.
    let text = unsafe { bytes(value) };
    let output = alloc(text.len() as u32);
    // SAFETY: helper call with an input string and a disjoint output of the same size.
    let len = unsafe {
        fetch::fetch_header_value(text.as_ptr(), text.len() as u32, output, text.len() as u32)
    };
    if len == INVALID {
        type_error(&[operation, b": \"", text, b"\" is an invalid header value."]);
        return None;
    }
    // SAFETY: the helper wrote `len` bytes.
    Some(unsafe { slice(output as u32, len) })
}

/// `headers.append(name, value)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_append(h: *mut u8, key: *mut u8, text: *mut u8) {
    let Some(key) = name(b"Headers.append", key) else {
        return;
    };
    let Some(text) = value(b"Headers.append", text) else {
        return;
    };
    headers(h).push(key, text);
}

/// `headers.set(name, value)`: replaces the first field of `name` and drops the others.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_set(h: *mut u8, key: *mut u8, text: *mut u8) {
    let Some(key) = name(b"Headers.set", key) else {
        return;
    };
    let Some(text) = value(b"Headers.set", text) else {
        return;
    };
    let list = headers(h);
    let mut replaced = Buffer::new();
    let mut found = false;
    for (field, (name, _)) in list.fields.as_slice().chunks_exact(16).zip(list.fields()) {
        if name != key {
            replaced.extend_from_slice(field);
        } else if !found {
            found = true;
            for word in [
                key.as_ptr() as u32,
                key.len() as u32,
                text.as_ptr() as u32,
                text.len() as u32,
            ] {
                replaced.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
    list.fields = replaced;
    if !found {
        list.push(key, text);
    }
}

/// `headers.get(name)`: the values joined with `, `, as UTF-8; null when absent.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_get(h: *mut u8, key: *mut u8) -> *mut u8 {
    let Some(key) = name(b"Headers.get", key) else {
        return core::ptr::null_mut();
    };
    let mut joined = Buffer::new();
    let mut found = false;
    for (name, value) in headers(h).fields() {
        if name != key {
            continue;
        }
        if found {
            joined.extend_from_slice(b", ");
        }
        found = true;
        for &byte in value {
            if byte < 0x80 {
                joined.push(byte);
            } else {
                joined.extend_from_slice(&[0xC0 | (byte >> 6), 0x80 | (byte & 0x3F)]);
            }
        }
    }
    if !found {
        return core::ptr::null_mut();
    }
    from_bytes(joined.as_slice())
}

/// `headers.has(name)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_has(h: *mut u8, key: *mut u8) -> u32 {
    let Some(key) = name(b"Headers.has", key) else {
        return 0;
    };
    u32::from(headers(h).fields().any(|(name, _)| name == key))
}

/// `headers.delete(name)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_delete(h: *mut u8, key: *mut u8) {
    let Some(key) = name(b"Headers.delete", key) else {
        return;
    };
    headers(h).retain(|name| name != key);
}

fn set_default(h: *mut u8, key: &'static [u8], text: &'static [u8]) {
    let list = headers(h);
    if !list.fields().any(|(name, _)| name == key) {
        list.push(key, text);
    }
}

// --- Bodies -------------------------------------------------------------------------------

/// The body both `Response` and `Request` start with.
#[repr(C)]
struct Body {
    /// 1 once read.
    used: u32,
    /// The snapshot, or null without a body.
    data: *mut u8,
    len: u32,
}

impl Body {
    fn snapshot(view: *mut u8) -> Body {
        if view.is_null() {
            return Body {
                used: 0,
                data: core::ptr::null_mut(),
                len: 0,
            };
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
    body(object).used
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
    if let Some(contents) = consume(object, promise) {
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
    if let Some(contents) = consume(object, promise) {
        let contents = contents.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(contents);
        let mut text = Buffer::new();
        for chunk in contents.utf8_chunks() {
            text.extend_from_slice(chunk.valid().as_bytes());
            if !chunk.invalid().is_empty() {
                text.extend_from_slice("\u{FFFD}".as_bytes());
            }
        }
        resolve(promise, from_bytes(text.as_slice()) as usize as u64);
    }
    promise
}

// --- Response -----------------------------------------------------------------------------

#[repr(C)]
struct Response {
    body: Body,
    status: f64,
    status_text: *mut u8,
    headers: *mut u8,
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
    if unsafe { fetch::fetch_status_text(reason.as_ptr(), reason.len() as u32) } == INVALID {
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
    let status = unsafe { fetch::fetch_url(input.as_ptr(), input.len() as u32, output, capacity) };
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
    let tag = unsafe { fetch::fetch_method(name.as_ptr(), name.len() as u32) };
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

// --- Writable.toWeb(process.stdout | process.stderr) --------------------------------------

/// A Web `WritableStream` over a console output.
#[repr(C)]
struct Writable {
    fd: u32,
    locked: u32,
    closed: u32,
}

/// Its `WritableStreamDefaultWriter`.
#[repr(C)]
struct Writer {
    stream: *mut Writable,
    released: u32,
}

/// `Writable.toWeb(process.stdout)` (`fd` 0) or `process.stderr` (1).
#[unsafe(no_mangle)]
pub extern "C" fn web_writable_new(fd: u32) -> *mut u8 {
    let stream = alloc(core::mem::size_of::<Writable>() as u32) as *mut Writable;
    // SAFETY: a fresh block.
    unsafe {
        stream.write(Writable {
            fd,
            locked: 0,
            closed: 0,
        })
    };
    stream as *mut u8
}

/// `stream.locked`.
#[unsafe(no_mangle)]
pub extern "C" fn web_writable_locked(stream: *mut u8) -> u32 {
    // SAFETY: compiled code passes writable streams.
    unsafe { (*(stream as *const Writable)).locked }
}

/// `stream.getWriter()`; `TypeError` while another writer holds the lock.
#[unsafe(no_mangle)]
pub extern "C" fn web_writable_get_writer(stream: *mut u8) -> *mut u8 {
    let stream = stream as *mut Writable;
    // SAFETY: compiled code passes writable streams.
    unsafe {
        if (*stream).locked != 0 {
            perry_abi::raise(TYPE_ERROR, "Invalid state: WritableStream is locked");
            return core::ptr::null_mut();
        }
        (*stream).locked = 1;
    }
    let writer = alloc(core::mem::size_of::<Writer>() as u32) as *mut Writer;
    // SAFETY: a fresh block.
    unsafe {
        writer.write(Writer {
            stream,
            released: 0,
        })
    };
    writer as *mut u8
}

/// The writer's stream, or a rejection of `promise` after `releaseLock()` or `close()`.
fn bound(writer: *mut u8, promise: *mut u8) -> Option<&'static mut Writable> {
    let writer = writer as *mut Writer;
    // SAFETY: compiled code passes writers; streams outlive them.
    unsafe {
        if (*writer).released != 0 {
            reject_with(
                promise,
                TYPE_ERROR,
                from_static("Invalid state: Writer is not bound to a WritableStream"),
            );
            return None;
        }
        let stream = &mut *(*writer).stream;
        if stream.closed != 0 {
            reject_with(
                promise,
                TYPE_ERROR,
                from_static("Invalid state: WritableStream is closed"),
            );
            return None;
        }
        Some(stream)
    }
}

/// `writer.write(bytes)`: the bytes join the output in order; the write is acknowledged when
/// the output accepted them.
///
/// # Safety
/// `writer` must be a writer and `view` a `Uint8Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn web_writer_write(writer: *mut u8, view: *mut u8) -> *mut u8 {
    let promise = promise_new();
    if let Some(stream) = bound(writer, promise) {
        // SAFETY: compiled code passes a `Uint8Array`.
        perry_abi::write(stream.fd, unsafe { view_bytes(view) });
        resolve(promise, perry_abi::UNDEFINED_BITS);
    }
    promise
}

/// `writer.close()`: later writes through this stream reject; the console output itself stays
/// open for other writers, as for Node's stdio.
#[unsafe(no_mangle)]
pub extern "C" fn web_writer_close(writer: *mut u8) -> *mut u8 {
    let promise = promise_new();
    if let Some(stream) = bound(writer, promise) {
        stream.closed = 1;
        resolve(promise, perry_abi::UNDEFINED_BITS);
    }
    promise
}

/// `writer.releaseLock()`.
#[unsafe(no_mangle)]
pub extern "C" fn web_writer_release_lock(writer: *mut u8) {
    let writer = writer as *mut Writer;
    // SAFETY: compiled code passes writers.
    unsafe {
        if (*writer).released == 0 {
            (*writer).released = 1;
            (*(*writer).stream).locked = 0;
        }
    }
}
