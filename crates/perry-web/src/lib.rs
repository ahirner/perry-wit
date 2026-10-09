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
    /// The previous write's promise, reused by leased writes.
    promise: *mut u8,
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
            promise: core::ptr::null_mut(),
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

/// `writer.write(bytes)`: the bytes join the output in order; the promise settles once the
/// host accepted them, so awaiting each write bounds the buffered output.
///
/// # Safety
/// `writer` must be a writer and `view` a `Uint8Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn web_writer_write(writer: *mut u8, view: *mut u8, leased: u32) -> *mut u8 {
    let promise = if leased != 0 {
        // SAFETY: compiled code passes writers.
        let previous = unsafe { &mut (*(writer as *mut Writer)).promise };
        *previous = perry_abi::promise_reuse(*previous);
        *previous
    } else {
        promise_new()
    };
    if let Some(stream) = bound(writer, promise) {
        // SAFETY: compiled code passes a `Uint8Array`.
        perry_abi::write_acknowledged(stream.fd, unsafe { view_bytes(view) }, promise);
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

// --- ReadableStream<Uint8Array> over a WIT stream<u8> -------------------------------------

/// Canonical built-ins for `stream<u8>`. `src/link` binds these placeholders to the built-ins
/// of a function of the WIT world that carries a `stream<u8>` (all such streams share one
/// type per component instance).
mod canon {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "perry:canon")]
    unsafe extern "C" {
        #[link_name = "[async-lower][stream-read]u8"]
        pub fn read(stream: u32, buffer: *mut u8, len: u32) -> u32;
        #[link_name = "[stream-cancel-read]u8"]
        pub fn cancel_read(stream: u32) -> u32;
        #[link_name = "[stream-drop-readable]u8"]
        pub fn drop_readable(stream: u32);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    mod host {
        pub unsafe fn read(_: u32, _: *mut u8, _: u32) -> u32 {
            unreachable!("streams exist in linked modules")
        }
        pub unsafe fn cancel_read(_: u32) -> u32 {
            unreachable!("streams exist in linked modules")
        }
        pub unsafe fn drop_readable(_: u32) {}
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub use host::*;
}

/// Bytes read per `read()`: the transfer size of the stream contracts.
const READ_SIZE: u32 = 8192;

const OPEN: u32 = 0;
const ENDED: u32 = 1;

/// A `ReadableStream<Uint8Array>` owning the readable end of a `stream<u8>`.
#[repr(C)]
struct Readable {
    /// The readable end, 0 once dropped (ended or cancelled).
    handle: u32,
    state: u32,
    locked: u32,
    task: *mut u8,
    next: *mut Readable,
}

/// A `ReadableStreamDefaultReader`.
#[repr(C)]
struct Reader {
    stream: *mut Readable,
    released: u32,
    /// The outstanding read's promise, while the host fills `buffer`, and whether it is
    /// leased.
    pending: *mut u8,
    pending_leased: u32,
    buffer: *mut u8,
    /// The previous read's promise, result and view, reused by leased reads.
    promise: *mut u8,
    result: *mut ReadResult,
    view: *mut u8,
    /// The buffer of leased reads, allocated once.
    leased: *mut u8,
    next: *mut Reader,
}

/// `{ done, value }`.
#[repr(C)]
struct ReadResult {
    done: u32,
    value: *mut u8,
}

struct Lists {
    readables: *mut Readable,
    waiting: *mut Reader,
}
struct Global(core::cell::UnsafeCell<Lists>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Global {}
static LISTS: Global = Global(core::cell::UnsafeCell::new(Lists {
    readables: core::ptr::null_mut(),
    waiting: core::ptr::null_mut(),
}));

fn lists() -> &'static mut Lists {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *LISTS.0.get() }
}

/// A stream over the readable end `handle` (an export's `stream<u8>` argument), owned by the
/// current task: [web_streams_close] drops it at the task's end if the program did not.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_new(handle: u32) -> *mut u8 {
    let stream = alloc(core::mem::size_of::<Readable>() as u32) as *mut Readable;
    // SAFETY: a fresh block, linked into the list of open streams.
    unsafe {
        stream.write(Readable {
            handle,
            state: OPEN,
            locked: 0,
            task: perry_abi::task(),
            next: lists().readables,
        })
    };
    lists().readables = stream;
    stream as *mut u8
}

fn readable<'a>(stream: *mut u8) -> &'a mut Readable {
    // SAFETY: compiled code passes readable streams.
    unsafe { &mut *(stream as *mut Readable) }
}

fn reader<'a>(r: *mut u8) -> &'a mut Reader {
    // SAFETY: compiled code passes readers.
    unsafe { &mut *(r as *mut Reader) }
}

/// Drop the readable end and forget the stream.
fn end(stream: &mut Readable) {
    stream.state = ENDED;
    if stream.handle != 0 {
        perry_abi::unjoin(stream.handle);
        // SAFETY: the readable end this stream owns.
        unsafe { canon::drop_readable(stream.handle) };
        stream.handle = 0;
    }
    let this = stream as *mut Readable;
    let mut link: *mut *mut Readable = &mut lists().readables;
    // SAFETY: the list holds live streams.
    unsafe {
        while !(*link).is_null() {
            if *link == this {
                *link = (*this).next;
                break;
            }
            link = &mut (**link).next;
        }
    }
}

/// `stream.locked`.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_locked(stream: *mut u8) -> u32 {
    readable(stream).locked
}

/// `stream.getReader()`; `TypeError` while another reader holds the lock.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_get_reader(stream: *mut u8) -> *mut u8 {
    let source = readable(stream);
    if source.locked != 0 {
        perry_abi::raise(TYPE_ERROR, "Invalid state: ReadableStream is locked");
        return core::ptr::null_mut();
    }
    source.locked = 1;
    let r = alloc(core::mem::size_of::<Reader>() as u32) as *mut Reader;
    // SAFETY: a fresh block.
    unsafe {
        r.write(Reader {
            stream: stream as *mut Readable,
            released: 0,
            pending: core::ptr::null_mut(),
            pending_leased: 0,
            buffer: core::ptr::null_mut(),
            promise: core::ptr::null_mut(),
            result: core::ptr::null_mut(),
            view: core::ptr::null_mut(),
            leased: core::ptr::null_mut(),
            next: core::ptr::null_mut(),
        })
    };
    r as *mut u8
}

/// `stream.cancel()` of an unlocked stream.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_cancel(stream: *mut u8) -> *mut u8 {
    let promise = promise_new();
    let source = readable(stream);
    if source.locked != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: ReadableStream is locked"),
        );
    } else {
        end(source);
        resolve(promise, perry_abi::UNDEFINED_BITS);
    }
    promise
}

/// Settle `promise` with `{ done, value }`; leased reads reuse the reader's objects.
fn settle_read(r: &mut Reader, promise: *mut u8, leased: bool, count: u32) {
    let (result, view) = if leased && !r.result.is_null() {
        (r.result, r.view)
    } else {
        (
            alloc(core::mem::size_of::<ReadResult>() as u32) as *mut ReadResult,
            core::ptr::null_mut(),
        )
    };
    let value = if count == 0 {
        core::ptr::null_mut()
    } else if view.is_null() {
        bytes_view(count, r.buffer)
    } else {
        // SAFETY: the reader's own view header.
        unsafe {
            *(view as *mut u32) = count;
            *(view.add(4) as *mut *mut u8) = r.buffer;
        }
        view
    };
    // SAFETY: a result object of this reader or a fresh one.
    unsafe {
        result.write(ReadResult {
            done: u32::from(count == 0),
            value,
        })
    };
    if leased {
        r.result = result;
        if !value.is_null() {
            r.view = value;
        }
    }
    resolve(promise, result as usize as u64);
}

/// A copy result: deliver the bytes, or end the stream when the writer is gone.
fn complete(r: &mut Reader, promise: *mut u8, leased: bool, code: u32) -> bool {
    let count = code >> 4;
    // SAFETY: readers point at their stream.
    let stream = unsafe { &mut *r.stream };
    if code & 0xF != COPY_COMPLETED {
        // The writer dropped its end (or the read was cancelled): no more bytes.
        end(stream);
    }
    if count == 0 && stream.state == OPEN {
        return false;
    }
    settle_read(r, promise, leased, count);
    true
}

const COPY_COMPLETED: u32 = 0;

/// `await reader.read()`. A leased read (the compiler proved that nothing from the previous
/// read is live) reuses the reader's buffer, promise and result; others get fresh ones.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_read(r: *mut u8, leased: u32) -> *mut u8 {
    let r = reader(r);
    let leased = leased != 0;
    let promise = if leased {
        perry_abi::promise_reuse(r.promise)
    } else {
        promise_new()
    };
    if leased {
        r.promise = promise;
    }
    if r.released != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: The reader is not attached to a stream"),
        );
        return promise;
    }
    if !r.pending.is_null() {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Concurrent reads of one reader are not supported"),
        );
        return promise;
    }
    loop {
        // SAFETY: readers point at their stream.
        let stream = unsafe { &mut *r.stream };
        if stream.state != OPEN {
            settle_read(r, promise, leased, 0);
            return promise;
        }
        r.buffer = if leased {
            if r.leased.is_null() {
                r.leased = alloc(READ_SIZE);
            }
            r.leased
        } else {
            alloc(READ_SIZE)
        };
        // SAFETY: the buffer stays valid until the copy completes.
        let code = unsafe { canon::read(stream.handle, r.buffer, READ_SIZE) };
        if code == perry_abi::BLOCKED {
            perry_abi::join(stream.handle);
            r.pending = promise;
            r.pending_leased = u32::from(leased);
            r.next = lists().waiting;
            lists().waiting = r;
            return promise;
        }
        if complete(r, promise, leased, code) {
            return promise;
        }
    }
}

/// Unlink `r` from the readers waiting on the host.
fn unwait(r: *mut Reader) {
    let mut link: *mut *mut Reader = &mut lists().waiting;
    // SAFETY: the list holds live readers.
    unsafe {
        while !(*link).is_null() {
            if *link == r {
                *link = (*r).next;
                break;
            }
            link = &mut (**link).next;
        }
    }
}

/// Finish the outstanding read of `r` with copy result `code`.
fn finish_pending(r: &mut Reader, code: u32) {
    let promise = core::mem::replace(&mut r.pending, core::ptr::null_mut());
    unwait(r);
    let leased = r.pending_leased != 0;
    if !complete(r, promise, leased, code) {
        // An empty copy of an open stream: read again for the same promise.
        // SAFETY: readers point at their stream.
        let handle = unsafe { (*r.stream).handle };
        // SAFETY: the buffer stays valid until the copy completes.
        let code = unsafe { canon::read(handle, r.buffer, READ_SIZE) };
        if code == perry_abi::BLOCKED {
            r.pending = promise;
            r.next = lists().waiting;
            lists().waiting = r;
        } else {
            finish_pending(r, code);
        }
    }
}

/// A callback event of the current task: complete the read waiting on `handle`. Returns 1 if
/// the event was one of a stream read.
#[unsafe(no_mangle)]
pub extern "C" fn web_event(event: u32, handle: u32, code: u32) -> u32 {
    if event != perry_abi::EVENT_STREAM_READ {
        return 0;
    }
    let mut r = lists().waiting;
    // SAFETY: waiting readers are live.
    unsafe {
        while !r.is_null() {
            if (*(*r).stream).handle == handle {
                finish_pending(&mut *r, code);
                return 1;
            }
            r = (*r).next;
        }
    }
    0
}

/// Cancel the outstanding read of `r`, if any: its promise settles with what was copied, or
/// with `done`.
fn cancel_pending(r: &mut Reader) {
    if r.pending.is_null() {
        return;
    }
    // SAFETY: readers point at their stream; the read is outstanding on its handle.
    let code = unsafe { canon::cancel_read((*r.stream).handle) };
    finish_pending(r, code);
}

/// `reader.cancel()`: ends the stream; an outstanding read settles first.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_cancel(r: *mut u8) -> *mut u8 {
    let r = reader(r);
    let promise = promise_new();
    if r.released != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: The reader is not attached to a stream"),
        );
        return promise;
    }
    cancel_pending(r);
    // SAFETY: readers point at their stream.
    end(unsafe { &mut *r.stream });
    resolve(promise, perry_abi::UNDEFINED_BITS);
    promise
}

/// `reader.releaseLock()`: an outstanding read rejects; the stream stays open for a new
/// reader.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_release_lock(r: *mut u8) {
    let r = reader(r);
    if r.released != 0 {
        return;
    }
    if !r.pending.is_null() {
        let promise = core::mem::replace(&mut r.pending, core::ptr::null_mut());
        unwait(r);
        // SAFETY: readers point at their stream; the read is outstanding on its handle.
        unsafe { canon::cancel_read((*r.stream).handle) };
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: Releasing reader"),
        );
    }
    r.released = 1;
    // SAFETY: readers point at their stream.
    unsafe { (*r.stream).locked = 0 };
}

/// `result.done`.
#[unsafe(no_mangle)]
pub extern "C" fn web_read_done(result: *mut u8) -> u32 {
    // SAFETY: compiled code passes read results.
    unsafe { (*(result as *const ReadResult)).done }
}

/// `result.value`: a `Uint8Array`, or null (`undefined`) when done.
#[unsafe(no_mangle)]
pub extern "C" fn web_read_value(result: *mut u8) -> *mut u8 {
    // SAFETY: compiled code passes read results.
    unsafe { (*(result as *const ReadResult)).value }
}

/// At the end of the current task: cancel its outstanding reads and drop the streams it
/// still owns, so no host handle outlives the export call.
#[unsafe(no_mangle)]
pub extern "C" fn web_streams_close() {
    let task = perry_abi::task();
    let mut r = lists().waiting;
    // SAFETY: listed readers and streams are live.
    unsafe {
        while !r.is_null() {
            let next = (*r).next;
            if (*(*r).stream).task == task {
                (*r).pending = core::ptr::null_mut();
                unwait(r);
                canon::cancel_read((*(*r).stream).handle);
            }
            r = next;
        }
        let mut stream = lists().readables;
        while !stream.is_null() {
            let next = (*stream).next;
            if (*stream).task == task {
                end(&mut *stream);
            }
            stream = next;
        }
    }
}
