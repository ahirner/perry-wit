//! Serving `wasi:http/handler@0.3.0`: the request a handler receives and the response it
//! returns.
//!
//! [web_serve_request] reads the request's method, scheme, authority, path and headers at
//! once and starts reading its body as a [Readable] (whose end counts once the trailers
//! future reports it intact, as for fetched bodies). [web_serve_respond] builds the response
//! with the headers added by [web_serve_add_header] and starts writing its body. The host
//! reads that body only after the export returned the response, so the task keeps running
//! after `task.return` until the body, the trailers and the transmission are done
//! ([web_serve_busy]); compiled code returns once ([web_serve_returned]) and then waits.
//!
//! Callbacks pass their events to [web_serve_event]; the body's reads use [web_event].
use crate::{
    readable::{Form, Readable, body_stream, complete, drain},
    *,
};

mod http {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "wasi:http/types@0.3.0")]
    unsafe extern "C" {
        #[link_name = "[constructor]fields"]
        pub(super) fn fields_new() -> u32;
        #[link_name = "[method]fields.append"]
        pub(super) fn fields_append(
            fields: u32,
            name: *const u8,
            nlen: u32,
            value: *const u8,
            vlen: u32,
            result: *mut u8,
        );
        #[link_name = "[method]fields.copy-all"]
        pub(super) fn fields_copy_all(fields: u32, result: *mut u32);
        #[link_name = "[resource-drop]fields"]
        pub(super) fn fields_drop(fields: u32);

        #[link_name = "[method]request.get-method"]
        pub(super) fn method(request: u32, result: *mut u8);
        #[link_name = "[method]request.get-scheme"]
        pub(super) fn scheme(request: u32, result: *mut u8);
        #[link_name = "[method]request.get-authority"]
        pub(super) fn authority(request: u32, result: *mut u8);
        #[link_name = "[method]request.get-path-with-query"]
        pub(super) fn path(request: u32, result: *mut u8);
        #[link_name = "[method]request.get-headers"]
        pub(super) fn headers(request: u32) -> u32;
        #[link_name = "[future-new-0][static]request.consume-body"]
        pub(super) fn outcome_new() -> u64;
        #[link_name = "[static]request.consume-body"]
        pub(super) fn consume_body(request: u32, outcome: u32, result: *mut u32);
        #[link_name = "[async-lower][future-write-0][static]request.consume-body"]
        pub(super) fn outcome_write(future: u32, value: *const u8) -> u32;
        #[link_name = "[future-cancel-write-0][static]request.consume-body"]
        pub(super) fn outcome_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-writable-0][static]request.consume-body"]
        pub(super) fn outcome_drop(future: u32);
        #[link_name = "[async-lower][future-read-2][static]request.consume-body"]
        pub(super) fn end_read(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-cancel-read-2][static]request.consume-body"]
        pub(super) fn end_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-readable-2][static]request.consume-body"]
        pub(super) fn end_drop(future: u32);

        #[link_name = "[future-new-1][static]response.new"]
        pub(super) fn trailers_new() -> u64;
        #[link_name = "[stream-new-0][static]response.new"]
        pub(super) fn contents_new() -> u64;
        #[link_name = "[static]response.new"]
        pub(super) fn response_new(
            headers: u32,
            has_contents: u32,
            contents: u32,
            trailers: u32,
            result: *mut u32,
        );
        #[link_name = "[method]response.set-status-code"]
        pub(super) fn set_status(response: u32, status: u32) -> u32;
        #[link_name = "[async-lower][stream-write-0][static]response.new"]
        pub(super) fn contents_write(stream: u32, data: *const u8, len: u32) -> u32;
        #[link_name = "[stream-cancel-write-0][static]response.new"]
        pub(super) fn contents_cancel(stream: u32) -> u32;
        #[link_name = "[stream-drop-writable-0][static]response.new"]
        pub(super) fn contents_drop(stream: u32);
        #[link_name = "[async-lower][future-write-1][static]response.new"]
        pub(super) fn trailers_write(future: u32, value: *const u8) -> u32;
        #[link_name = "[future-cancel-write-1][static]response.new"]
        pub(super) fn trailers_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-writable-1][static]response.new"]
        pub(super) fn trailers_drop(future: u32);
        #[link_name = "[async-lower][future-read-2][static]response.new"]
        pub(super) fn transmit_read(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-cancel-read-2][static]response.new"]
        pub(super) fn transmit_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-readable-2][static]response.new"]
        pub(super) fn transmit_drop(future: u32);
    }

    /// `wasi:http` exists in linked modules only; host builds never call it.
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc, clippy::too_many_arguments)]
    mod host {
        macro_rules! absent {
            ($($name:ident($($arg:ty),*) $(-> $result:ty)?;)*) => {
                $(pub(crate) unsafe fn $name($(_: $arg),*) $(-> $result)? {
                    unreachable!("wasi:http runs in linked modules")
                })*
            };
        }
        absent! {
            fields_new() -> u32;
            fields_append(u32, *const u8, u32, *const u8, u32, *mut u8);
            fields_copy_all(u32, *mut u32);
            fields_drop(u32);
            method(u32, *mut u8);
            scheme(u32, *mut u8);
            authority(u32, *mut u8);
            path(u32, *mut u8);
            headers(u32) -> u32;
            outcome_new() -> u64;
            consume_body(u32, u32, *mut u32);
            outcome_write(u32, *const u8) -> u32;
            outcome_cancel(u32) -> u32;
            outcome_drop(u32);
            end_read(u32, *mut u8) -> u32;
            end_cancel(u32) -> u32;
            end_drop(u32);
            trailers_new() -> u64;
            contents_new() -> u64;
            response_new(u32, u32, u32, u32, *mut u32);
            set_status(u32, u32) -> u32;
            contents_write(u32, *const u8, u32) -> u32;
            contents_cancel(u32) -> u32;
            contents_drop(u32);
            trailers_write(u32, *const u8) -> u32;
            trailers_cancel(u32) -> u32;
            trailers_drop(u32);
            transmit_read(u32, *mut u8) -> u32;
            transmit_cancel(u32) -> u32;
            transmit_drop(u32);
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) use host::*;
}

/// Result areas: `result<option<trailers>, error-code>` and friends, the payload at 8.
const RESULT_SIZE: u32 = 40;

const METHODS: [&str; 10] = [
    "get", "head", "post", "put", "delete", "connect", "options", "trace", "patch", "other",
];
const SCHEMES: [&str; 3] = ["http", "https", "other"];

/// One exchange: the request's parts and the waitables the task still owns (0 when none).
struct Serve {
    pin: u32,
    method: *mut u8,
    method_other: *mut u8,
    scheme: *mut u8,
    scheme_other: *mut u8,
    authority: *mut u8,
    path: *mut u8,
    /// `copy-all` entries: name and value (data, length) pairs.
    headers: perry_abi::Buffer,
    header_count: u32,
    body: *mut Readable,
    /// The consume-body outcome future (writable end, written `Ok`) and the request's
    /// trailers future (readable end, read to learn whether the body arrived intact).
    outcome: u32,
    outcome_blocked: bool,
    end: u32,
    ended: *mut u8,
    /// The response's headers, contents stream (writable end) with the bytes still to
    /// write, trailers future (writable end) and transmission future (readable end).
    fields: u32,
    contents: u32,
    contents_blocked: bool,
    data: *const u8,
    left: u32,
    trailers: u32,
    trailers_blocked: bool,
    transmit: u32,
    transmitted: *mut u8,
    zero: *mut u8,
    /// `task.return` was called.
    returned: bool,
    task: *mut u8,
    next: *mut Serve,
}

struct Ops(core::cell::UnsafeCell<*mut Serve>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Ops {}
static OPS: Ops = Ops(core::cell::UnsafeCell::new(core::ptr::null_mut()));

fn ops() -> &'static mut *mut Serve {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *OPS.0.get() }
}

fn serve<'a>(s: *mut u8) -> &'a mut Serve {
    // SAFETY: compiled code passes exchanges from [web_serve_request].
    unsafe { &mut *(s as *mut Serve) }
}

/// The current task's exchange, if it has one.
fn current() -> Option<&'static mut Serve> {
    let task = perry_abi::task();
    let mut op = *ops();
    // SAFETY: listed exchanges are live.
    unsafe {
        while !op.is_null() {
            if (*op).task == task {
                return Some(&mut *op);
            }
            op = (*op).next;
        }
    }
    None
}

fn word(area: *const u8, offset: usize) -> u32 {
    // SAFETY: callers read result areas the host wrote.
    unsafe { *(area.add(offset) as *const u32) }
}

/// A string the host allocated (`cabi_realloc`, in the task region) at `area + offset`.
fn host_string(area: *const u8, offset: usize) -> *mut u8 {
    // SAFETY: the host wrote a string's data and length.
    unsafe {
        let data = word(area, offset) as usize as *mut u8;
        let value = perry_abi::adopt(data, word(area, offset + 4));
        perry_abi::release_canonical(data);
        value
    }
}

/// `requestInfo(request)`: read the request's parts and start reading its body.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_request(request: f64) -> *mut u8 {
    let request = request as u32;
    let area = alloc(RESULT_SIZE);
    let op = perry_abi::object!(Serve; method, method_other, scheme, scheme_other, authority, path, headers, body, ended, data, transmitted, zero);
    let zero = alloc(RESULT_SIZE);
    // SAFETY: canonical calls on the request this export owns; fresh areas.
    unsafe {
        core::ptr::write_bytes(zero, 0, RESULT_SIZE as usize);
        http::method(request, area);
        let tag = (*area as usize).min(METHODS.len() - 1);
        let method_other = if tag == METHODS.len() - 1 {
            host_string(area, 4)
        } else {
            core::ptr::null_mut()
        };
        http::scheme(request, area);
        let (scheme, scheme_other) = if *area == 0 {
            (core::ptr::null_mut(), core::ptr::null_mut())
        } else {
            let tag = (*area.add(4) as usize).min(SCHEMES.len() - 1);
            let other = if tag == SCHEMES.len() - 1 {
                host_string(area, 8)
            } else {
                core::ptr::null_mut()
            };
            (from_static(SCHEMES[tag]), other)
        };
        http::authority(request, area);
        let authority = if *area == 0 {
            core::ptr::null_mut()
        } else {
            host_string(area, 4)
        };
        http::path(request, area);
        let path = if *area == 0 {
            core::ptr::null_mut()
        } else {
            host_string(area, 4)
        };
        let fields = http::headers(request);
        let mut list = [0u32; 2];
        http::fields_copy_all(fields, list.as_mut_ptr());
        http::fields_drop(fields);
        let outcome = http::outcome_new();
        let mut ends = [0u32; 2];
        http::consume_body(request, outcome as u32, ends.as_mut_ptr());
        op.write(Serve {
            pin: perry_abi::pin(op.cast()),
            method: from_static(METHODS[tag]),
            method_other,
            scheme,
            scheme_other,
            authority,
            path,
            headers: crate::headers::adopt_field_buffer(list[0] as usize as *mut u8, list[1]),
            header_count: list[1],
            body: body_stream(ends[0]),
            outcome: (outcome >> 32) as u32,
            outcome_blocked: false,
            end: ends[1],
            ended: alloc(RESULT_SIZE),
            fields: 0,
            contents: 0,
            contents_blocked: false,
            data: core::ptr::null(),
            left: 0,
            trailers: 0,
            trailers_blocked: false,
            transmit: 0,
            transmitted: alloc(RESULT_SIZE),
            zero,
            returned: false,
            task: perry_abi::task(),
            next: *ops(),
        });
        *ops() = op;
        let s = &mut *op;
        write_outcome(s);
        read_end(s);
    }
    op as *mut u8
}

/// `await requestBody(incoming)`: the whole body.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_body(s: *mut u8) -> *mut u8 {
    let promise = promise_new();
    drain(serve(s).body, Form::Bytes, promise);
    promise
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_method(s: *mut u8) -> *mut u8 {
    serve(s).method
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_method_other(s: *mut u8) -> *mut u8 {
    serve(s).method_other
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_has_scheme(s: *mut u8) -> u32 {
    u32::from(!serve(s).scheme.is_null())
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_scheme(s: *mut u8) -> *mut u8 {
    let scheme = serve(s).scheme;
    if scheme.is_null() {
        from_static("")
    } else {
        scheme
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_scheme_other(s: *mut u8) -> *mut u8 {
    serve(s).scheme_other
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_authority(s: *mut u8) -> *mut u8 {
    serve(s).authority
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_path(s: *mut u8) -> *mut u8 {
    serve(s).path
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_header_count(s: *mut u8) -> f64 {
    f64::from(serve(s).header_count)
}

/// Entry `index` of the request headers: (name data, name length, value data, value
/// length).
fn header(s: *mut u8, index: f64) -> [u32; 4] {
    let s = serve(s);
    let index = (index as u32).min(s.header_count.saturating_sub(1)) as usize;
    // SAFETY: `copy-all` wrote `header_count` entries of four words.
    unsafe { *(s.headers.as_slice().as_ptr().add(index * 16) as *const [u32; 4]) }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_header_name(s: *mut u8, index: f64) -> *mut u8 {
    let [data, len, ..] = header(s, index);
    // SAFETY: the host allocated the name in the task region.
    unsafe { perry_abi::adopt(data as usize as *mut u8, len) }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_header_value(s: *mut u8, index: f64) -> *mut u8 {
    let [.., data, len] = header(s, index);
    bytes_view(len, data as usize as *mut u8)
}

/// Add a response header; an invalid name or value throws `TypeError`.
///
/// # Safety
/// `name` must be a string and `value` a `Uint8Array`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn web_serve_add_header(s: *mut u8, name: *mut u8, value: *mut u8) {
    let s = serve(s);
    // SAFETY: guaranteed by the caller.
    let (name, value) = unsafe { (bytes(name), view_bytes(value)) };
    let area = alloc(24);
    // SAFETY: canonical calls on fields this exchange owns.
    unsafe {
        if s.fields == 0 {
            s.fields = http::fields_new();
        }
        http::fields_append(
            s.fields,
            name.as_ptr(),
            name.len() as u32,
            value.as_ptr(),
            value.len() as u32,
            area,
        );
        if *area != 0 {
            type_error(&[b"Invalid header: ", name]);
        }
    }
}

/// Create the response (status, the added headers, `body`) and start writing its body;
/// returns the `response` handle for `task.return`.
///
/// # Safety
/// `body` must be a `Uint8Array`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn web_serve_respond(s: *mut u8, status: f64, body: *mut u8) -> f64 {
    let s = serve(s);
    // SAFETY: guaranteed by the caller; the bytes live in the task region until written.
    let body = unsafe { view_bytes(body) };
    if !(100.0..=999.0).contains(&status) || status != f64::from(status as u32) {
        perry_abi::raise(RANGE_ERROR, "Invalid status code");
        return 0.0;
    }
    // SAFETY: canonical calls on handles this exchange owns.
    unsafe {
        if s.fields == 0 {
            s.fields = http::fields_new();
        }
        let trailers = http::trailers_new();
        let contents = if body.is_empty() {
            0
        } else {
            http::contents_new()
        };
        let mut ends = [0u32; 2];
        http::response_new(
            s.fields,
            u32::from(contents != 0),
            contents as u32,
            trailers as u32,
            ends.as_mut_ptr(),
        );
        s.fields = 0;
        let (response, transmit) = (ends[0], ends[1]);
        if http::set_status(response, status as u32) != 0 {
            perry_abi::raise(RANGE_ERROR, "Invalid status code");
        }
        s.contents = (contents >> 32) as u32;
        s.data = body.as_ptr();
        s.left = body.len() as u32;
        s.trailers = (trailers >> 32) as u32;
        s.transmit = transmit;
        write_contents(s);
        write_trailers(s);
        read_transmit(s);
        f64::from(response)
    }
}

/// Write the response body until the host blocks; drop the stream when it is all written.
fn write_contents(s: &mut Serve) {
    while s.contents != 0 {
        if s.left == 0 {
            perry_abi::unjoin(s.contents);
            // SAFETY: the writable end this exchange owns.
            unsafe { http::contents_drop(s.contents) };
            s.contents = 0;
            return;
        }
        // SAFETY: the body lives in the task region until the write completes.
        let code = unsafe { http::contents_write(s.contents, s.data, s.left) };
        if code == perry_abi::BLOCKED {
            s.contents_blocked = true;
            perry_abi::join(s.contents);
            return;
        }
        written(s, code);
    }
}

fn written(s: &mut Serve, code: u32) {
    let count = code >> 4;
    // SAFETY: within the body.
    s.data = unsafe { s.data.add(count as usize) };
    s.left -= count;
    if code & 0xF != 0 {
        // The host stopped reading: the rest is not sent.
        s.left = 0;
    }
}

/// Write `Ok(none)` to the response's trailers future.
fn write_trailers(s: &mut Serve) {
    // SAFETY: a zeroed value area that outlives the write.
    let code = unsafe { http::trailers_write(s.trailers, s.zero) };
    if code == perry_abi::BLOCKED {
        s.trailers_blocked = true;
        perry_abi::join(s.trailers);
    } else {
        drop_trailers(s);
    }
}

fn drop_trailers(s: &mut Serve) {
    perry_abi::unjoin(s.trailers);
    // SAFETY: the writable end this exchange owns.
    unsafe { http::trailers_drop(s.trailers) };
    s.trailers = 0;
    s.trailers_blocked = false;
}

/// Wait for the end of the transmission: the host keeps the exchange alive while it is held.
fn read_transmit(s: &mut Serve) {
    // SAFETY: the readable end this exchange owns; the area outlives the read.
    let code = unsafe { http::transmit_read(s.transmit, s.transmitted) };
    if code == perry_abi::BLOCKED {
        perry_abi::join(s.transmit);
    } else {
        drop_transmit(s);
    }
}

fn drop_transmit(s: &mut Serve) {
    perry_abi::unjoin(s.transmit);
    // SAFETY: the readable end this exchange owns.
    unsafe { http::transmit_drop(s.transmit) };
    s.transmit = 0;
}

/// Report `Ok` on the consume-body outcome future at once.
fn write_outcome(s: &mut Serve) {
    // SAFETY: a zeroed value area that outlives the write.
    let code = unsafe { http::outcome_write(s.outcome, s.zero) };
    if code == perry_abi::BLOCKED {
        s.outcome_blocked = true;
        perry_abi::join(s.outcome);
    } else {
        drop_outcome(s);
    }
}

fn drop_outcome(s: &mut Serve) {
    perry_abi::unjoin(s.outcome);
    // SAFETY: the writable end this exchange owns.
    unsafe { http::outcome_drop(s.outcome) };
    s.outcome = 0;
    s.outcome_blocked = false;
}

/// Read the request's trailers future: it tells whether the body arrived intact.
fn read_end(s: &mut Serve) {
    // SAFETY: the readable end this exchange owns; the area outlives the read.
    let code = unsafe { http::end_read(s.end, s.ended) };
    if code == perry_abi::BLOCKED {
        perry_abi::join(s.end);
    } else {
        ended(s);
    }
}

fn ended(s: &mut Serve) {
    let (failed, some, trailers) = (
        word(s.ended, 0) & 0xFF != 0,
        word(s.ended, 8) & 0xFF != 0,
        word(s.ended, 12),
    );
    if !failed && some {
        // SAFETY: trailers handed to this exchange.
        unsafe { http::fields_drop(trailers) };
    }
    perry_abi::unjoin(s.end);
    // SAFETY: the readable end this exchange owns.
    unsafe { http::end_drop(s.end) };
    s.end = 0;
    complete(s.body, !failed);
}

/// A callback event of the current task: advance the exchange waiting on `handle`. Returns
/// 1 if it was one of an exchange.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_event(event: u32, handle: u32, code: u32) -> u32 {
    let Some(s) = current() else { return 0 };
    match event {
        perry_abi::EVENT_STREAM_WRITE if s.contents == handle => {
            s.contents_blocked = false;
            written(s, code);
            write_contents(s);
        }
        EVENT_FUTURE_WRITE if s.trailers == handle => drop_trailers(s),
        EVENT_FUTURE_WRITE if s.outcome == handle => drop_outcome(s),
        perry_abi::EVENT_FUTURE_READ if s.end == handle => ended(s),
        perry_abi::EVENT_FUTURE_READ if s.transmit == handle => drop_transmit(s),
        _ => return 0,
    }
    1
}

const EVENT_FUTURE_WRITE: u32 = 5;

/// Whether the current task already returned its response.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_returned() -> u32 {
    u32::from(current().is_some_and(|s| s.returned))
}

/// Note that the current task returned its response.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_mark_returned() {
    if let Some(s) = current() {
        s.returned = true;
    }
}

/// Whether the current task still writes its response (body, trailers) or waits for its
/// transmission.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_busy() -> u32 {
    u32::from(current().is_some_and(|s| s.contents != 0 || s.trailers != 0 || s.transmit != 0))
}

/// At the end of the current task: cancel what its exchange still waits for, drop its
/// handles and forget it.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_close() {
    let task = perry_abi::task();
    let mut link: *mut *mut Serve = ops();
    // SAFETY: listed exchanges are live; the handles are theirs.
    unsafe {
        while !(*link).is_null() {
            let s = &mut **link;
            if s.task != task {
                link = &mut s.next;
                continue;
            }
            *link = s.next;
            if s.outcome != 0 {
                if s.outcome_blocked {
                    perry_abi::unjoin(s.outcome);
                    perry_abi::acknowledged(|| http::outcome_cancel(s.outcome));
                }
                drop_outcome(s);
            }
            if s.end != 0 {
                perry_abi::unjoin(s.end);
                perry_abi::acknowledged(|| http::end_cancel(s.end));
                http::end_drop(s.end);
                s.end = 0;
            }
            if s.contents != 0 {
                perry_abi::unjoin(s.contents);
                if s.contents_blocked {
                    perry_abi::acknowledged(|| http::contents_cancel(s.contents));
                }
                http::contents_drop(s.contents);
                s.contents = 0;
            }
            if s.trailers != 0 {
                if s.trailers_blocked {
                    perry_abi::unjoin(s.trailers);
                    perry_abi::acknowledged(|| http::trailers_cancel(s.trailers));
                }
                drop_trailers(s);
            }
            if s.transmit != 0 {
                perry_abi::unjoin(s.transmit);
                perry_abi::acknowledged(|| http::transmit_cancel(s.transmit));
                drop_transmit(s);
            }
            if s.fields != 0 {
                http::fields_drop(s.fields);
                s.fields = 0;
            }
            perry_abi::unpin(core::mem::take(&mut s.pin));
            link = ops();
        }
    }
}
