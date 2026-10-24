//! `fetch(url, { method, headers, body })` over `wasi:http/client@0.3.0`.
//!
//! An operation builds the request using the URL, method, and header rules of `perry-fetch`,
//! sends it as an async-lowered call, and settles its promise with a `Response` once the
//! response head arrives. The body is a [crate::readable::Readable] over the response's
//! `stream<u8>`. Its end is confirmed once the trailers future reports that the body arrived
//! intact, and a truncated body rejects the final read with `TypeError: terminated` as in Node.
//! Request bodies are written through the request's contents stream. The request's transmission
//! future is held and read until the exchange finishes so the host continues driving the
//! connection for the response body. Every handle is dropped when the operation finishes or by
//! [web_fetch_close] at the end of the task.
//!
//! Callbacks pass their events to [web_fetch_event], and the readable side uses [crate::readable::web_event].
use crate::{
    body::{Body, Response},
    http_exchange::{Exchange, RESULT_SIZE, Request},
    *,
};

mod http {
    pub(super) use crate::headers::wasi::{
        fields_append, fields_copy_all, fields_drop, fields_new,
    };
    perry_rt_sys::wasm_imports! { "wasi:http/types@0.3.0";
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
    }
    perry_rt_sys::wasm_imports! { "wasi:http/client@0.3.0";
        #[link_name = "[async-lower]send"]
        pub(super) fn send(request: u32, result: *mut u8) -> u32;
    }
}

/// One `fetch` call: the waitables it still owns (0 when none).
struct Fetch {
    pin: u32,
    promise: *mut u8,
    url: *mut u8,
    /// The `send` subtask, while it runs.
    sending: u32,
    exchange: Exchange<Request>,
    sent: *mut u8,
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
    let body = if view.is_null() {
        &[][..]
    } else {
        // SAFETY: compiled code passes a `Uint8Array`.
        unsafe { view_bytes(view) }
    };
    let (output, tag) =
        match unsafe { validate_request_init(url, method, !body.is_empty(), text, h) } {
            Ok(ok) => ok,
            Err(msg) => {
                reject_with(promise, TYPE_ERROR, msg);
                return promise;
            }
        };
    // SAFETY: URL normalization wrote [secure, authority, path, path, length, length+fragment].
    let (secure, authority, path, end) = unsafe {
        (
            word(output, 0),
            word(output, 4),
            word(output, 8),
            word(output, 16),
        )
    };
    let name = unsafe { bytes(method) };
    let mut copy = Buffer::new();
    copy.extend_from_slice(body);

    let op = perry_rt_sys::object!(Fetch; promise, url, sent, exchange.data, exchange.body, exchange.ended, exchange.transmitted, exchange.zero);
    let exchange = Exchange::new();
    // SAFETY: fresh blocks; every canonical call below takes live arguments.
    unsafe {
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
                reject_with(promise, TYPE_ERROR, join_bytes(&[b"Invalid header: ", key]));
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
            pin: perry_rt_sys::pin(op.cast()),
            promise,
            url,
            sending: 0,
            exchange,
            sent: alloc(RESULT_SIZE),
            task: perry_rt_sys::task(),
            next: *ops(),
        });
        *ops() = op;
        let fetch = &mut *op;
        fetch.exchange.write(
            (contents >> 32) as u32,
            (trailers >> 32) as u32,
            transmit,
            copy.as_slice(),
        );
        let status = http::send(request, fetch.sent);
        if status & 0xF == SUBTASK_RETURNED {
            sent(fetch);
        } else {
            fetch.sending = status >> 4;
            perry_rt_sys::join(fetch.sending);
        }
        finish_if_idle(fetch);
    }
    promise
}

/// `send` returned: settle the promise with the response, or reject.
fn sent(op: &mut Fetch) {
    // SAFETY: `send` wrote `result<response, error-code>` to the area.
    let (failed, response) = unsafe { (*op.sent != 0, *(op.sent.add(8) as *const u32)) };
    if failed {
        op.exchange.cancel_writes();
        let category = unsafe { error_category(*op.sent.add(8)) };
        let url = unsafe { bytes(op.url) };
        perry_rt_sys::reject_native(
            op.promise,
            TYPE_ERROR,
            join_bytes(&[b"fetch failed: ", category.as_bytes(), b": ", url]),
            category,
            category,
            "url",
            url,
        );
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
        op.exchange.receive(response);
        let r = perry_rt_sys::object!(Response; body.data, body.stream, status_text, headers);
        r.write(Response {
            body: Body::streamed(op.exchange.body),
            status: f64::from(status),
            status_text: from_static(""),
            headers,
        });
        resolve(op.promise, r as usize as u64);
        op.exchange.read();
    }
}

/// Forget the operation once it owns no waitable.
fn finish_if_idle(op: &mut Fetch) {
    if op.sending != 0 || op.exchange.busy() {
        return;
    }
    perry_rt_sys::unpin(core::mem::take(&mut op.pin));
    unsafe { unlink_node!(ops(), op as *mut Fetch) };
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
                perry_rt_sys::EVENT_SUBTASK if fetch.sending == handle => {
                    if code == SUBTASK_RETURNED {
                        perry_rt_sys::unjoin(handle);
                        perry_rt_sys::subtask_drop(handle);
                        fetch.sending = 0;
                        sent(fetch);
                    }
                    true
                }
                _ => fetch.exchange.event(event, handle, code),
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

/// Unsettled fetch promises and final body reads belonging to the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_fetch_pending() -> u32 {
    let task = perry_rt_sys::task();
    let mut op = *ops();
    let mut pending = 0;
    // SAFETY: listed fetches retain their bodies until all completion handles are released.
    unsafe {
        while !op.is_null() {
            let fetch = &*op;
            if fetch.task == task {
                pending += u32::from(
                    fetch.sending != 0 && !perry_rt_sys::cancellation_owned(fetch.promise),
                );
                pending += u32::from(crate::readable::detached_completion(fetch.exchange.body));
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
    let task = perry_rt_sys::task();
    let mut op = *ops();
    // SAFETY: listed operations are live; the handles are theirs.
    unsafe {
        while !op.is_null() {
            let fetch = &mut *op;
            if fetch.task == task {
                if fetch.sending != 0 {
                    perry_rt_sys::unjoin(fetch.sending);
                    let status = perry_rt_sys::subtask_cancel(fetch.sending);
                    perry_rt_sys::subtask_drop(fetch.sending);
                    fetch.sending = 0;
                    if status == SUBTASK_RETURNED {
                        sent(fetch);
                    }
                }
                fetch.exchange.close();
                finish_if_idle(fetch);
                op = *ops();
            } else {
                op = fetch.next;
            }
        }
    }
}

fn error_category(case: u8) -> &'static str {
    const CATEGORIES: &[&str] = &[
        "DNS-timeout",
        "DNS-error",
        "destination-not-found",
        "destination-unavailable",
        "destination-IP-prohibited",
        "destination-IP-unroutable",
        "connection-refused",
        "connection-terminated",
        "connection-timeout",
        "connection-read-timeout",
        "connection-write-timeout",
        "connection-limit-reached",
        "TLS-protocol-error",
        "TLS-certificate-error",
        "TLS-alert-received",
        "HTTP-request-denied",
        "HTTP-request-length-required",
        "HTTP-request-body-size",
        "HTTP-request-method-invalid",
        "HTTP-request-URI-invalid",
        "HTTP-request-URI-too-long",
        "HTTP-request-header-section-size",
        "HTTP-request-header-size",
        "HTTP-request-trailer-section-size",
        "HTTP-request-trailer-size",
        "HTTP-response-incomplete",
        "HTTP-response-header-section-size",
        "HTTP-response-header-size",
        "HTTP-response-body-size",
        "HTTP-response-trailer-section-size",
        "HTTP-response-trailer-size",
        "HTTP-response-transfer-coding",
        "HTTP-response-content-coding",
        "HTTP-response-timeout",
        "HTTP-upgrade-failed",
        "HTTP-protocol-error",
        "loop-detected",
        "configuration-error",
        "internal-error",
    ];
    CATEGORIES
        .get(case as usize)
        .copied()
        .unwrap_or("internal-error")
}
