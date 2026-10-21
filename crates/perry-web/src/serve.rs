//! Serving `wasi:http/handler@0.3.0`: the request a handler receives and the response it
//! returns.
//!
//! [web_serve_request] reads the request's method, scheme, authority, path, and headers at
//! once and starts reading its body as a [crate::readable::Readable] whose end is confirmed
//! once the trailers future reports it intact. [web_serve_respond] builds the response
//! with the headers added by [web_serve_add_header] and starts writing its body. The host
//! reads that body after the export returns the response, so the task keeps running
//! after `task.return` until the body, the trailers, and the transmission complete
//! ([web_serve_busy]). Compiled code returns the response once ([web_serve_returned]) and then waits.
//!
//! Callbacks pass their events to [web_serve_event], and body reads use [crate::readable::web_event].
use crate::{
    http_exchange::{Exchange, RESULT_SIZE, Response},
    readable::{Form, drain},
    *,
};

mod http {
    pub(super) use crate::headers::wasi::{
        fields_append, fields_copy_all, fields_drop, fields_new,
    };
    perry_abi::wasm_imports! { "wasi:http/types@0.3.0";
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
    }
}

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
    exchange: Exchange<Response>,
    fields: u32,
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
    let op = perry_abi::object!(Serve; method, method_other, scheme, scheme_other, authority, path, headers, exchange.body, exchange.ended, exchange.data, exchange.transmitted, exchange.zero);
    let mut exchange = Exchange::new();
    // SAFETY: canonical calls on the request this export owns; fresh areas.
    unsafe {
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
        exchange.receive(request);
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
            exchange,
            fields: 0,
            returned: false,
            task: perry_abi::task(),
            next: *ops(),
        });
        *ops() = op;
        let s = &mut *op;
        s.exchange.read();
    }
    op as *mut u8
}

/// `await requestBody(incoming)`: the whole body.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_body(s: *mut u8) -> *mut u8 {
    let promise = promise_new();
    drain(serve(s).exchange.body, Form::Bytes, promise);
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

/// Add a response header, throwing `TypeError` when the name or value is invalid.
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

/// Create the response from the status, accumulated headers, and `body`, start writing its
/// body stream, and return the `response` handle for `task.return`.
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
        s.exchange.write(
            (contents >> 32) as u32,
            (trailers >> 32) as u32,
            transmit,
            body,
        );
        f64::from(response)
    }
}

/// A callback event of the current task: advance the exchange waiting on `handle`. Returns
/// 1 if it was one of an exchange.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn web_serve_event(event: u32, handle: u32, code: u32) -> u32 {
    let Some(s) = current() else { return 0 };
    u32::from(s.exchange.event(event, handle, code))
}

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
    u32::from(current().is_some_and(|s| s.exchange.writing()))
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
            s.exchange.close();
            if s.fields != 0 {
                http::fields_drop(s.fields);
                s.fields = 0;
            }
            perry_abi::unpin(core::mem::take(&mut s.pin));
            link = ops();
        }
    }
}
