//! `fetch(url, { method, headers, body })` over `wasi:http/client@0.3.0`.
//!
//! An operation builds the request (URL, method and header rules of the fetch helper), sends
//! it as an async-lowered call, and settles its promise with a `Response` once the head
//! arrived. The body is a [Readable] over the response's `stream<u8>`; its end counts once the
//! trailers future reports that the body arrived intact (otherwise the last read rejects with
//! `TypeError: terminated`, as in Node). Request bodies are written through the request's
//! contents stream. The request's transmission future is held (and read) until the exchange
//! ends: the host drives the connection only while it lives, so dropping it early cuts off
//! response bodies. Every handle is dropped when the operation finishes, or by
//! [web_fetch_close] at the end of the task.
//!
//! Callbacks pass their events to [web_fetch_event]; the readable side uses [web_event].
use crate::{
    body::{Body, Response},
    readable::{Readable, body_stream, complete},
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

        #[link_name = "[future-new-1][static]request.new"]
        pub(super) fn trailers_new() -> u64;
        #[link_name = "[stream-new-0][static]request.new"]
        pub(super) fn contents_new() -> u64;
        #[link_name = "[static]request.new"]
        pub(super) fn request_new(
            headers: u32,
            has_contents: u32,
            contents: u32,
            trailers: u32,
            has_options: u32,
            options: u32,
            result: *mut u32,
        );
        #[link_name = "[async-lower][future-write-1][static]request.new"]
        pub(super) fn trailers_write(future: u32, value: *const u8) -> u32;
        #[link_name = "[future-cancel-write-1][static]request.new"]
        pub(super) fn trailers_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-writable-1][static]request.new"]
        pub(super) fn trailers_drop(future: u32);
        #[link_name = "[async-lower][stream-write-0][static]request.new"]
        pub(super) fn contents_write(stream: u32, data: *const u8, len: u32) -> u32;
        #[link_name = "[stream-cancel-write-0][static]request.new"]
        pub(super) fn contents_cancel(stream: u32) -> u32;
        #[link_name = "[stream-drop-writable-0][static]request.new"]
        pub(super) fn contents_drop(stream: u32);
        #[link_name = "[async-lower][future-read-2][static]request.new"]
        pub(super) fn transmit_read(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-cancel-read-2][static]request.new"]
        pub(super) fn transmit_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-readable-2][static]request.new"]
        pub(super) fn transmit_drop(future: u32);
        #[link_name = "[method]request.set-method"]
        pub(super) fn set_method(request: u32, tag: u32, other: *const u8, len: u32) -> u32;
        #[link_name = "[method]request.set-scheme"]
        pub(super) fn set_scheme(
            request: u32,
            some: u32,
            tag: u32,
            other: *const u8,
            len: u32,
        ) -> u32;
        #[link_name = "[method]request.set-authority"]
        pub(super) fn set_authority(request: u32, some: u32, text: *const u8, len: u32) -> u32;
        #[link_name = "[method]request.set-path-with-query"]
        pub(super) fn set_path(request: u32, some: u32, text: *const u8, len: u32) -> u32;

        #[link_name = "[method]response.get-status-code"]
        pub(super) fn status(response: u32) -> u32;
        #[link_name = "[method]response.get-headers"]
        pub(super) fn headers(response: u32) -> u32;
        #[link_name = "[future-new-0][static]response.consume-body"]
        pub(super) fn outcome_new() -> u64;
        #[link_name = "[static]response.consume-body"]
        pub(super) fn consume_body(response: u32, outcome: u32, result: *mut u32);
        #[link_name = "[async-lower][future-write-0][static]response.consume-body"]
        pub(super) fn outcome_write(future: u32, value: *const u8) -> u32;
        #[link_name = "[future-cancel-write-0][static]response.consume-body"]
        pub(super) fn outcome_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-writable-0][static]response.consume-body"]
        pub(super) fn outcome_drop(future: u32);
        #[link_name = "[async-lower][future-read-2][static]response.consume-body"]
        pub(super) fn end_read(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-cancel-read-2][static]response.consume-body"]
        pub(super) fn end_cancel(future: u32) -> u32;
        #[link_name = "[future-drop-readable-2][static]response.consume-body"]
        pub(super) fn end_drop(future: u32);
    }

    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "wasi:http/client@0.3.0")]
    unsafe extern "C" {
        #[link_name = "[async-lower]send"]
        pub(super) fn send(request: u32, result: *mut u8) -> u32;
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
            trailers_new() -> u64;
            contents_new() -> u64;
            request_new(u32, u32, u32, u32, u32, u32, *mut u32);
            trailers_write(u32, *const u8) -> u32;
            trailers_cancel(u32) -> u32;
            trailers_drop(u32);
            contents_write(u32, *const u8, u32) -> u32;
            contents_cancel(u32) -> u32;
            contents_drop(u32);
            transmit_read(u32, *mut u8) -> u32;
            transmit_cancel(u32) -> u32;
            transmit_drop(u32);
            set_method(u32, u32, *const u8, u32) -> u32;
            set_scheme(u32, u32, u32, *const u8, u32) -> u32;
            set_authority(u32, u32, *const u8, u32) -> u32;
            set_path(u32, u32, *const u8, u32) -> u32;
            status(u32) -> u32;
            headers(u32) -> u32;
            outcome_new() -> u64;
            consume_body(u32, u32, *mut u32);
            outcome_write(u32, *const u8) -> u32;
            outcome_cancel(u32) -> u32;
            outcome_drop(u32);
            end_read(u32, *mut u8) -> u32;
            end_cancel(u32) -> u32;
            end_drop(u32);
            send(u32, *mut u8) -> u32;
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) use host::*;
}

/// `result<option<trailers>, error-code>` and `result<response, error-code>`: the payload at 8.
const RESULT_SIZE: u32 = 40;

/// One `fetch` call: the waitables it still owns (0 when none).
struct Fetch {
    pin: u32,
    promise: *mut u8,
    /// The `send` subtask, while it runs.
    sending: u32,
    /// The request's contents stream (writable end) and the bytes still to write.
    contents: u32,
    contents_blocked: bool,
    data: *const u8,
    left: u32,
    /// The request trailers future (writable end), written `Ok(none)`.
    trailers: u32,
    trailers_blocked: bool,
    /// The response's trailers future (readable end), read to learn whether the body ended
    /// intact, and the consume-body outcome future (writable end), written `Ok`.
    end: u32,
    outcome: u32,
    outcome_blocked: bool,
    body: *mut Readable,
    /// The request's transmission future (readable end), read until the exchange is over:
    /// the host keeps the connection running only while it is held.
    transmit: u32,
    /// Result areas: `send`'s, the trailers', the transmission's, and the values written (all
    /// zero: `Ok`).
    sent: *mut u8,
    ended: *mut u8,
    transmitted: *mut u8,
    zero: *mut u8,
    task: *mut u8,
    next: *mut Fetch,
}

struct Ops(core::cell::UnsafeCell<*mut Fetch>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Ops {}
static OPS: Ops = Ops(core::cell::UnsafeCell::new(core::ptr::null_mut()));

fn ops() -> &'static mut *mut Fetch {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *OPS.0.get() }
}

const SUBTASK_RETURNED: u32 = 2;

/// `fetch(url, init)`: `method` a string, `h` a header list, `view` a body or null (`text`
/// marks a string body).
///
/// # Safety
/// `url` and `method` must be strings, `h` a header list and `view` null or a `Uint8Array`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn web_fetch(
    url: *mut u8,
    method: *mut u8,
    h: *mut u8,
    view: *mut u8,
    text: u32,
) -> *mut u8 {
    let promise = promise_new();
    // SAFETY: compiled code passes strings.
    let input = unsafe { bytes(url) };
    let capacity = 32 + input.len() as u32 * 3 + 16;
    let output = alloc(capacity);
    // SAFETY: helper call with an input string and a fresh disjoint output.
    let status = unsafe { helper::fetch_url(input.as_ptr(), input.len() as u32, output, capacity) };
    if status != 0 {
        let mut message = Buffer::new();
        message.extend_from_slice(b"Failed to parse URL from ");
        message.extend_from_slice(input);
        reject_with(promise, TYPE_ERROR, from_bytes(message.as_slice()));
        return promise;
    }
    // SAFETY: the helper wrote [secure, authority, path, path, length, length+fragment].
    let word = |index: usize| unsafe { *(output.add(index * 4) as *const u32) };
    let (secure, authority, path, end) = (word(0), word(1), word(2), word(4));
    // SAFETY: compiled code passes strings.
    let name = unsafe { bytes(method) };
    // SAFETY: helper call on an input string.
    let tag = unsafe { helper::fetch_method(name.as_ptr(), name.len() as u32) };
    if tag == u32::MAX {
        let mut message = Buffer::new();
        for part in [b"'".as_slice(), name, b"' is not a valid HTTP method."] {
            message.extend_from_slice(part);
        }
        reject_with(promise, TYPE_ERROR, from_bytes(message.as_slice()));
        return promise;
    }
    let body = if view.is_null() {
        &[][..]
    } else {
        // SAFETY: compiled code passes a `Uint8Array`.
        unsafe { view_bytes(view) }
    };
    if !body.is_empty() && matches!(tag, 0 | 1) {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Request with GET/HEAD method cannot have body."),
        );
        return promise;
    }
    if text != 0 {
        crate::headers::set_default(h, b"content-type", b"text/plain;charset=UTF-8");
    }
    let mut copy = Buffer::new();
    copy.extend_from_slice(body);

    let op = perry_abi::object!(Fetch; promise, data, body, sent, ended, transmitted, zero);
    let zero = alloc(RESULT_SIZE);
    // SAFETY: fresh blocks; every canonical call below takes live arguments.
    unsafe {
        core::ptr::write_bytes(zero, 0, RESULT_SIZE as usize);
        let fields = http::fields_new();
        let area = alloc(24);
        for (key, value) in crate::headers::fields(h) {
            http::fields_append(
                fields,
                key.as_ptr(),
                key.len() as u32,
                value.as_ptr(),
                value.len() as u32,
                area,
            );
            if *area != 0 {
                http::fields_drop(fields);
                let mut message = Buffer::new();
                message.extend_from_slice(b"Invalid header: ");
                message.extend_from_slice(key);
                reject_with(promise, TYPE_ERROR, from_bytes(message.as_slice()));
                return promise;
            }
        }
        let trailers = http::trailers_new();
        let contents = if copy.is_empty() {
            0
        } else {
            http::contents_new()
        };
        let mut ends = [0u32; 2];
        http::request_new(
            fields,
            u32::from(contents != 0),
            contents as u32,
            trailers as u32,
            0,
            0,
            ends.as_mut_ptr(),
        );
        let (request, transmit) = (ends[0], ends[1]);
        let other = if tag == 9 { name } else { &[][..] };
        http::set_method(request, tag, other.as_ptr(), other.len() as u32);
        http::set_scheme(request, 1, secure, core::ptr::null(), 0);
        let text = slice(output as u32 + 32, end);
        let authority = &text[authority as usize..path as usize];
        let target = &text[path as usize..];
        http::set_authority(request, 1, authority.as_ptr(), authority.len() as u32);
        http::set_path(request, 1, target.as_ptr(), target.len() as u32);
        op.write(Fetch {
            pin: perry_abi::pin(op.cast()),
            promise,
            sending: 0,
            contents: (contents >> 32) as u32,
            contents_blocked: false,
            data: copy.as_slice().as_ptr(),
            left: copy.len() as u32,
            trailers: (trailers >> 32) as u32,
            trailers_blocked: false,
            end: 0,
            outcome: 0,
            outcome_blocked: false,
            body: core::ptr::null_mut(),
            transmit,
            sent: alloc(RESULT_SIZE),
            ended: alloc(RESULT_SIZE),
            transmitted: alloc(RESULT_SIZE),
            zero,
            task: perry_abi::task(),
            next: *ops(),
        });
        *ops() = op;
        let fetch = &mut *op;
        write_contents(fetch);
        write_trailers(fetch);
        read_transmit(fetch);
        let status = http::send(request, fetch.sent);
        if status & 0xF == SUBTASK_RETURNED {
            sent(fetch);
        } else {
            fetch.sending = status >> 4;
            perry_abi::join(fetch.sending);
        }
        finish_if_idle(fetch);
    }
    promise
}

/// Write the request body until the host blocks; drop the stream when it is all written.
fn write_contents(op: &mut Fetch) {
    while op.contents != 0 {
        if op.left == 0 {
            perry_abi::unjoin(op.contents);
            // SAFETY: the writable end this fetch owns.
            unsafe { http::contents_drop(op.contents) };
            op.contents = 0;
            return;
        }
        // SAFETY: the copy lives in the task region until the write completes.
        let code = unsafe { http::contents_write(op.contents, op.data, op.left) };
        if code == perry_abi::BLOCKED {
            op.contents_blocked = true;
            perry_abi::join(op.contents);
            return;
        }
        contents_written(op, code);
    }
}

fn contents_written(op: &mut Fetch, code: u32) {
    let count = code >> 4;
    // SAFETY: within the copy.
    op.data = unsafe { op.data.add(count as usize) };
    op.left -= count;
    if code & 0xF != 0 {
        // The host stopped reading: the rest is not sent.
        op.left = 0;
    }
}

/// Write `Ok(none)` to the request's trailers future.
fn write_trailers(op: &mut Fetch) {
    if op.trailers == 0 {
        return;
    }
    // SAFETY: a zeroed value area that outlives the write.
    let code = unsafe { http::trailers_write(op.trailers, op.zero) };
    if code == perry_abi::BLOCKED {
        op.trailers_blocked = true;
        perry_abi::join(op.trailers);
    } else {
        drop_trailers(op);
    }
}

fn drop_trailers(op: &mut Fetch) {
    perry_abi::unjoin(op.trailers);
    // SAFETY: the writable end this fetch owns.
    unsafe { http::trailers_drop(op.trailers) };
    op.trailers = 0;
    op.trailers_blocked = false;
}

/// `send` returned: settle the promise with the response, or reject.
fn sent(op: &mut Fetch) {
    // SAFETY: `send` wrote `result<response, error-code>` to the area.
    let (failed, response) = unsafe { (*op.sent != 0, *(op.sent.add(8) as *const u32)) };
    if failed {
        cancel_writes(op);
        reject_with(op.promise, TYPE_ERROR, from_static("fetch failed"));
        return;
    }
    // SAFETY: canonical calls on the response this fetch owns.
    unsafe {
        let status = http::status(response);
        let fields = http::headers(response);
        let mut list = [0u32; 2];
        http::fields_copy_all(fields, list.as_mut_ptr());
        http::fields_drop(fields);
        let headers = crate::headers::adopt_fields(list[0] as *mut u8, list[1]);
        let outcome = http::outcome_new();
        let mut ends = [0u32; 2];
        http::consume_body(response, outcome as u32, ends.as_mut_ptr());
        op.outcome = (outcome >> 32) as u32;
        op.body = body_stream(ends[0]);
        op.end = ends[1];
        let r = perry_abi::object!(Response; body.data, body.stream, status_text, headers);
        r.write(Response {
            body: Body::streamed(op.body),
            status: f64::from(status),
            status_text: from_static(""),
            headers,
        });
        resolve(op.promise, r as usize as u64);
        write_outcome(op);
        read_end(op);
    }
}

/// Report `Ok` on the consume-body outcome future at once: the host pipes it while the body
/// streams, and a writable end must be written before it is dropped.
fn write_outcome(op: &mut Fetch) {
    // SAFETY: a zeroed value area that outlives the write.
    let code = unsafe { http::outcome_write(op.outcome, op.zero) };
    if code == perry_abi::BLOCKED {
        op.outcome_blocked = true;
        perry_abi::join(op.outcome);
    } else {
        drop_outcome(op);
    }
}

/// Read the response trailers future: it tells whether the body arrived intact.
fn read_end(op: &mut Fetch) {
    // SAFETY: the readable end this fetch owns; the area outlives the read.
    let code = unsafe { http::end_read(op.end, op.ended) };
    if code == perry_abi::BLOCKED {
        perry_abi::join(op.end);
    } else {
        ended(op);
    }
}

/// The body's outcome arrived: report it to the stream and confirm consumption.
fn ended(op: &mut Fetch) {
    // SAFETY: the read wrote `result<option<trailers>, error-code>`.
    let (failed, some, trailers) = unsafe {
        (
            *op.ended != 0,
            *op.ended.add(8) != 0,
            *(op.ended.add(12) as *const u32),
        )
    };
    if !failed && some {
        // SAFETY: trailers handed to this fetch.
        unsafe { http::fields_drop(trailers) };
    }
    perry_abi::unjoin(op.end);
    // SAFETY: the readable end this fetch owns.
    unsafe { http::end_drop(op.end) };
    op.end = 0;
    complete(op.body, !failed);
}

/// Wait for the end of the exchange; the outcome itself reaches readers through the trailers.
fn read_transmit(op: &mut Fetch) {
    // SAFETY: the readable end this fetch owns; the area outlives the read.
    let code = unsafe { http::transmit_read(op.transmit, op.transmitted) };
    if code == perry_abi::BLOCKED {
        perry_abi::join(op.transmit);
    } else {
        drop_transmit(op);
    }
}

fn drop_transmit(op: &mut Fetch) {
    perry_abi::unjoin(op.transmit);
    // SAFETY: the readable end this fetch owns.
    unsafe { http::transmit_drop(op.transmit) };
    op.transmit = 0;
}

fn drop_outcome(op: &mut Fetch) {
    perry_abi::unjoin(op.outcome);
    // SAFETY: the writable end this fetch owns.
    unsafe { http::outcome_drop(op.outcome) };
    op.outcome = 0;
    op.outcome_blocked = false;
}

/// Stop writing the request body and trailers (the request failed or the task ends).
fn cancel_writes(op: &mut Fetch) {
    // SAFETY: writable ends this fetch owns.
    unsafe {
        if op.contents != 0 {
            perry_abi::unjoin(op.contents);
            if op.contents_blocked {
                perry_abi::acknowledged(|| http::contents_cancel(op.contents));
            }
            http::contents_drop(op.contents);
            op.contents = 0;
        }
        if op.trailers != 0 {
            if op.trailers_blocked {
                perry_abi::unjoin(op.trailers);
                perry_abi::acknowledged(|| http::trailers_cancel(op.trailers));
            }
            drop_trailers(op);
        }
    }
}

/// Forget the operation once it owns no waitable.
fn finish_if_idle(op: &mut Fetch) {
    if op.sending != 0
        || op.contents != 0
        || op.trailers != 0
        || op.end != 0
        || op.outcome != 0
        || op.transmit != 0
    {
        return;
    }
    perry_abi::unpin(core::mem::take(&mut op.pin));
    let this = op as *mut Fetch;
    let mut link: *mut *mut Fetch = ops();
    // SAFETY: the list holds live operations.
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

/// A callback event of the current task: advance the fetch waiting on `handle`. Returns 1 if
/// it was one of a fetch.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_fetch_event(event: u32, handle: u32, code: u32) -> u32 {
    let mut op = *ops();
    // SAFETY: listed operations are live.
    unsafe {
        while !op.is_null() {
            let fetch = &mut *op;
            let next = fetch.next;
            let handled = match event {
                perry_abi::EVENT_SUBTASK if fetch.sending == handle => {
                    if code == SUBTASK_RETURNED {
                        perry_abi::unjoin(handle);
                        perry_abi::subtask_drop(handle);
                        fetch.sending = 0;
                        sent(fetch);
                    }
                    true
                }
                perry_abi::EVENT_STREAM_WRITE if fetch.contents == handle => {
                    fetch.contents_blocked = false;
                    contents_written(fetch, code);
                    write_contents(fetch);
                    true
                }
                EVENT_FUTURE_WRITE if fetch.trailers == handle => {
                    drop_trailers(fetch);
                    true
                }
                EVENT_FUTURE_WRITE if fetch.outcome == handle => {
                    drop_outcome(fetch);
                    true
                }
                perry_abi::EVENT_FUTURE_READ if fetch.end == handle => {
                    ended(fetch);
                    true
                }
                perry_abi::EVENT_FUTURE_READ if fetch.transmit == handle => {
                    drop_transmit(fetch);
                    true
                }
                _ => false,
            };
            if handled {
                finish_if_idle(fetch);
                return 1;
            }
            op = next;
        }
    }
    0
}

const EVENT_FUTURE_WRITE: u32 = 5;

/// Unsettled fetch promises and final body reads belonging to the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_fetch_pending() -> u32 {
    let task = perry_abi::task();
    let mut op = *ops();
    let mut pending = 0;
    // SAFETY: listed fetches retain their bodies until all completion handles are released.
    unsafe {
        while !op.is_null() {
            let fetch = &*op;
            if fetch.task == task {
                pending += u32::from(fetch.sending != 0);
                pending += u32::from(crate::readable::pending_completion(fetch.body));
            }
            op = fetch.next;
        }
    }
    pending
}

/// At the end of the current task: cancel what its fetches still wait for and drop their
/// handles.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_fetch_close() {
    let task = perry_abi::task();
    let mut op = *ops();
    // SAFETY: listed operations are live; the handles are theirs.
    unsafe {
        while !op.is_null() {
            let fetch = &mut *op;
            if fetch.task == task {
                if fetch.sending != 0 {
                    perry_abi::unjoin(fetch.sending);
                    let status = perry_abi::subtask_cancel(fetch.sending);
                    perry_abi::subtask_drop(fetch.sending);
                    fetch.sending = 0;
                    if status == SUBTASK_RETURNED {
                        sent(fetch);
                    }
                }
                cancel_writes(fetch);
                if fetch.end != 0 {
                    perry_abi::unjoin(fetch.end);
                    perry_abi::acknowledged(|| http::end_cancel(fetch.end));
                    http::end_drop(fetch.end);
                    fetch.end = 0;
                }
                if fetch.outcome != 0 {
                    if fetch.outcome_blocked {
                        perry_abi::unjoin(fetch.outcome);
                        perry_abi::acknowledged(|| http::outcome_cancel(fetch.outcome));
                    }
                    drop_outcome(fetch);
                }
                if fetch.transmit != 0 {
                    perry_abi::unjoin(fetch.transmit);
                    perry_abi::acknowledged(|| http::transmit_cancel(fetch.transmit));
                    drop_transmit(fetch);
                }
                finish_if_idle(fetch);
                op = *ops();
            } else {
                op = fetch.next;
            }
        }
    }
}
