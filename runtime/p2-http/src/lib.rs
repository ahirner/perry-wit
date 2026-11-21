//! Native WASI HTTP 0.2 backend for Perry's fetch and response-body protocol.
#![no_std]
extern crate alloc;
use alloc::{string::ToString, vec::Vec};
use core::{mem::ManuallyDrop, ptr};
use perry_p2_sys::{
    self as sys, BLOCKED, DROPPED, EVENT_STREAM_READ, EVENT_SUBTASK, Operation, wasip2,
};
use wasip2::http::{outgoing_handler, types::*};
use wasip2::io::streams::{InputStream, OutputStream, StreamError};

unsafe fn fields(handle: u32) -> ManuallyDrop<Fields> {
    ManuallyDrop::new(unsafe { Fields::from_handle(handle) })
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_http_fields_new() -> u32 {
    Fields::new().take_handle()
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_fields_drop(handle: u32) {
    drop(unsafe { Fields::from_handle(handle) });
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_fields_append(
    handle: u32,
    name: *const u8,
    n: u32,
    value: *const u8,
    len: u32,
    result: *mut u8,
) {
    let result_value = unsafe { fields(handle) }.append(unsafe { sys::text(name, n) }, unsafe {
        sys::bytes(value, len)
    });
    unsafe {
        *result = u8::from(result_value.is_err());
    }
    if let Err(error) = result_value {
        unsafe {
            *result.add(1) = match error {
                HeaderError::InvalidSyntax => 0,
                HeaderError::Forbidden => 1,
                HeaderError::Immutable => 2,
            };
        }
    }
}
unsafe fn lower_bytes(bytes: &[u8]) -> u32 {
    let area = unsafe { sys::cabi_realloc(ptr::null_mut(), 0, 1, bytes.len() as u32) };
    if !bytes.is_empty() {
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), area, bytes.len()) };
    }
    area as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_fields_entries(handle: u32, result: *mut u32) {
    let entries = unsafe { fields(handle) }.entries();
    let area = unsafe { sys::cabi_realloc(ptr::null_mut(), 0, 4, (entries.len() * 16) as u32) }
        .cast::<u32>();
    for (i, (name, value)) in entries.iter().enumerate() {
        unsafe {
            area.add(i * 4).write(lower_bytes(name.as_bytes()));
            area.add(i * 4 + 1).write(name.len() as u32);
            area.add(i * 4 + 2).write(lower_bytes(value));
            area.add(i * 4 + 3).write(value.len() as u32);
        }
    }
    unsafe {
        *result = area as u32;
        *result.add(1) = entries.len() as u32;
    }
}

struct Request {
    request: Option<OutgoingRequest>,
    contents: u32,
    transmit: u32,
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_http_contents_new() -> u64 {
    let id = Operation::new(0, 0).with(Vec::<u8>::new(), None).register();
    u64::from(id) | (u64::from(id) << 32)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_contents_write(
    handle: u32,
    data: *const u8,
    len: u32,
) -> u32 {
    unsafe { sys::payload::<Vec<u8>>(sys::op(handle)) }
        .extend_from_slice(unsafe { sys::bytes(data, len) });
    len << 4
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_http_contents_drop(_: u32) {
    // The request owns the readable end until send takes the buffered body.
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_request_new(
    headers: u32,
    has_contents: u32,
    contents: u32,
    trailers: u32,
    has_options: u32,
    _: u32,
    result: *mut u32,
) {
    assert_eq!(has_options, 0);
    unsafe { sys::p2_drop(trailers) };
    let transmit = sys::completion(40, false);
    let request = Request {
        request: Some(OutgoingRequest::new(unsafe {
            Fields::from_handle(headers)
        })),
        contents: if has_contents != 0 { contents } else { 0 },
        transmit,
    };
    let id = Operation::new(0, 0).with(request, None).register();
    unsafe {
        *result = id;
        *result.add(1) = transmit;
    }
}
unsafe fn request(handle: u32) -> &'static OutgoingRequest {
    unsafe { sys::payload::<Request>(sys::op(handle)) }
        .request
        .as_ref()
        .unwrap()
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_set_method(
    handle: u32,
    tag: u32,
    other: *const u8,
    len: u32,
) -> u32 {
    let method = match tag {
        0 => Method::Get,
        1 => Method::Head,
        2 => Method::Post,
        3 => Method::Put,
        4 => Method::Delete,
        5 => Method::Connect,
        6 => Method::Options,
        7 => Method::Trace,
        8 => Method::Patch,
        9 => Method::Other(unsafe { sys::text(other, len) }.to_string()),
        _ => panic!("method tag"),
    };
    u32::from(unsafe { request(handle) }.set_method(&method).is_err())
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_set_scheme(
    handle: u32,
    some: u32,
    tag: u32,
    other: *const u8,
    len: u32,
) -> u32 {
    let scheme = match tag {
        0 => Scheme::Http,
        1 => Scheme::Https,
        2 => Scheme::Other(unsafe { sys::text(other, len) }.to_string()),
        _ => panic!("scheme tag"),
    };
    u32::from(
        unsafe { request(handle) }
            .set_scheme(if some == 0 { None } else { Some(&scheme) })
            .is_err(),
    )
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_set_authority(
    handle: u32,
    some: u32,
    text: *const u8,
    len: u32,
) -> u32 {
    u32::from(
        unsafe { request(handle) }
            .set_authority(if some == 0 {
                None
            } else {
                Some(unsafe { sys::text(text, len) })
            })
            .is_err(),
    )
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_set_path(
    handle: u32,
    some: u32,
    text: *const u8,
    len: u32,
) -> u32 {
    u32::from(
        unsafe { request(handle) }
            .set_path_with_query(if some == 0 {
                None
            } else {
                Some(unsafe { sys::text(text, len) })
            })
            .is_err(),
    )
}

struct Upload {
    stream: Option<OutputStream>,
    body: Option<OutgoingBody>,
    bytes: Vec<u8>,
    offset: usize,
}
unsafe extern "C" fn upload_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    let upload = unsafe { sys::payload::<Upload>(operation) };
    while upload.offset < upload.bytes.len() {
        let stream = upload.stream.as_ref().unwrap();
        match stream.check_write() {
            Ok(0) => {
                if operation.wait.is_none() {
                    operation.wait = Some(stream.subscribe());
                }
                return;
            }
            Ok(capacity) => {
                let n = (upload.bytes.len() - upload.offset).min(capacity as usize);
                if stream
                    .write(&upload.bytes[upload.offset..upload.offset + n])
                    .is_err()
                {
                    operation.error = Some(7);
                    break;
                }
                upload.offset += n;
            }
            Err(_) => {
                operation.error = Some(7);
                break;
            }
        }
    }
    drop(operation.wait.take());
    drop(upload.stream.take());
    if let Some(body) = upload.body.take()
        && OutgoingBody::finish(body, None).is_err()
    {
        operation.error = Some(7);
    }
    unsafe { sys::finish(operation) };
}
struct Sending {
    future: Option<FutureIncomingResponse>,
}
fn error_case(error: &ErrorCode) -> u8 {
    match error {
        ErrorCode::DnsTimeout => 0,
        ErrorCode::DnsError(_) => 1,
        ErrorCode::DestinationNotFound => 2,
        ErrorCode::DestinationUnavailable => 3,
        ErrorCode::DestinationIpProhibited => 4,
        ErrorCode::DestinationIpUnroutable => 5,
        ErrorCode::ConnectionRefused => 6,
        ErrorCode::ConnectionTerminated => 7,
        ErrorCode::ConnectionTimeout => 8,
        ErrorCode::ConnectionReadTimeout => 9,
        ErrorCode::ConnectionWriteTimeout => 10,
        ErrorCode::ConnectionLimitReached => 11,
        ErrorCode::TlsProtocolError => 12,
        ErrorCode::TlsCertificateError => 13,
        ErrorCode::TlsAlertReceived(_) => 14,
        ErrorCode::HttpRequestDenied => 15,
        ErrorCode::HttpRequestLengthRequired => 16,
        ErrorCode::HttpRequestBodySize(_) => 17,
        ErrorCode::HttpRequestMethodInvalid => 18,
        ErrorCode::HttpRequestUriInvalid => 19,
        ErrorCode::HttpRequestUriTooLong => 20,
        ErrorCode::HttpRequestHeaderSectionSize(_) => 21,
        ErrorCode::HttpRequestHeaderSize(_) => 22,
        ErrorCode::HttpRequestTrailerSectionSize(_) => 23,
        ErrorCode::HttpRequestTrailerSize(_) => 24,
        ErrorCode::HttpResponseIncomplete => 25,
        ErrorCode::HttpResponseHeaderSectionSize(_) => 26,
        ErrorCode::HttpResponseHeaderSize(_) => 27,
        ErrorCode::HttpResponseBodySize(_) => 28,
        ErrorCode::HttpResponseTrailerSectionSize(_) => 29,
        ErrorCode::HttpResponseTrailerSize(_) => 30,
        ErrorCode::HttpResponseTransferCoding(_) => 31,
        ErrorCode::HttpResponseContentCoding(_) => 32,
        ErrorCode::HttpResponseTimeout => 33,
        ErrorCode::HttpUpgradeFailed => 34,
        ErrorCode::HttpProtocolError => 35,
        ErrorCode::LoopDetected => 36,
        ErrorCode::ConfigurationError => 37,
        ErrorCode::InternalError(_) => 38,
    }
}
unsafe fn send_result(result: *mut u8, response: Result<IncomingResponse, ErrorCode>) {
    unsafe { ptr::write_bytes(result, 0, 40) };
    match response {
        Ok(response) => {
            let id = Operation::new(0, 0).with(Some(response), None).register();
            unsafe { result.add(8).cast::<u32>().write(id) };
        }
        Err(error) => unsafe {
            *result = 1;
            *result.add(8) = error_case(&error);
        },
    }
}
unsafe extern "C" fn send_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    let sending = unsafe { sys::payload::<Sending>(operation) };
    if let Some(result) = sending.future.as_ref().unwrap().get() {
        drop(operation.wait.take());
        drop(sending.future.take());
        unsafe { send_result(operation.result, result.expect("response read once")) };
        operation.ready = true;
    } else if operation.wait.is_none() {
        operation.wait = Some(sending.future.as_ref().unwrap().subscribe());
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_send(handle: u32, result: *mut u8) -> u32 {
    let req = unsafe { sys::payload::<Request>(sys::op(handle)) };
    let request = req.request.take().unwrap();
    let transmit = req.transmit;
    let body = if req.contents != 0 {
        let bytes = core::mem::take(unsafe { sys::payload::<Vec<u8>>(sys::op(req.contents)) });
        unsafe { sys::p2_drop(req.contents) };
        let body = request.body().expect("one outgoing body");
        let stream = body.write().expect("one outgoing stream");
        Some(Upload {
            stream: Some(stream),
            body: Some(body),
            bytes,
            offset: 0,
        })
    } else {
        None
    };
    unsafe { sys::p2_drop(handle) };
    match outgoing_handler::handle(request, None) {
        Ok(future) => {
            let transmission = unsafe { sys::op(transmit) };
            if let Some(body) = body {
                let replacement = Operation::new(0, 0).with(body, Some(upload_step));
                transmission.payload = replacement.payload;
                transmission.destroy = replacement.destroy;
                transmission.step = replacement.step;
                core::mem::forget(replacement);
                unsafe { upload_step(transmission) };
            } else {
                unsafe { sys::finish(transmission) };
            }
            let mut operation = Operation::new(EVENT_SUBTASK, 2).with(
                Sending {
                    future: Some(future),
                },
                Some(send_step),
            );
            operation.result = result;
            operation.register() << 4
        }
        Err(error) => {
            drop(body);
            unsafe {
                sys::finish(sys::op(transmit));
                send_result(result, Err(error));
            }
            2
        }
    }
}
unsafe fn response(handle: u32) -> &'static IncomingResponse {
    unsafe { sys::payload::<Option<IncomingResponse>>(sys::op(handle)) }
        .as_ref()
        .unwrap()
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_status(handle: u32) -> u32 {
    u32::from(unsafe { response(handle) }.status())
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_headers(handle: u32) -> u32 {
    unsafe { response(handle) }.headers().take_handle()
}

struct Input {
    stream: Option<InputStream>,
    body: Option<IncomingBody>,
    response: Option<IncomingResponse>,
    end: u32,
    pending: Option<(*mut u8, u32)>,
    failed: bool,
}
struct End {
    future: Option<FutureTrailers>,
    failed: bool,
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_consume(handle: u32, outcome: u32, result: *mut u32) {
    unsafe { sys::p2_drop(outcome) };
    let response = unsafe { sys::payload::<Option<IncomingResponse>>(sys::op(handle)) }
        .take()
        .unwrap();
    let body = response.consume().expect("one incoming body");
    let stream = body.stream().expect("one incoming stream");
    let end = sys::completion(40, false);
    unsafe {
        sys::op(end).refs += 1;
    }
    let id = Operation::new(EVENT_STREAM_READ, 0)
        .with(
            Input {
                stream: Some(stream),
                body: Some(body),
                response: Some(response),
                end,
                pending: None,
                failed: false,
            },
            Some(read_step),
        )
        .register();
    unsafe {
        sys::p2_drop(handle);
        *result = id;
        *result.add(1) = end;
    }
}
unsafe fn read(operation: &mut Operation, buffer: *mut u8, len: u32) -> u32 {
    let input = unsafe { sys::payload::<Input>(operation) };
    match input.stream.as_ref().unwrap().read(u64::from(len)) {
        Ok(bytes) if !bytes.is_empty() => {
            unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), buffer, bytes.len()) };
            (bytes.len() as u32) << 4
        }
        Ok(_) => {
            input.pending = Some((buffer, len));
            operation.ready = false;
            operation.notified = false;
            if operation.wait.is_none() {
                operation.wait = Some(input.stream.as_ref().unwrap().subscribe());
            }
            BLOCKED
        }
        Err(StreamError::Closed) => DROPPED,
        Err(_) => {
            input.failed = true;
            DROPPED
        }
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_read(handle: u32, buffer: *mut u8, len: u32) -> u32 {
    unsafe { read(sys::op(handle), buffer, len) }
}
unsafe extern "C" fn read_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    let Some((buffer, len)) = (unsafe { sys::payload::<Input>(operation) }).pending else {
        return;
    };
    let code = unsafe { read(operation, buffer, len) };
    if code != BLOCKED {
        unsafe { sys::payload::<Input>(operation) }.pending = None;
        operation.code = code;
        unsafe { sys::finish(operation) };
    }
}
unsafe extern "C" fn end_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    let end = unsafe { sys::payload::<End>(operation) };
    if let Some(result) = end.future.as_ref().unwrap().get() {
        drop(operation.wait.take());
        drop(end.future.take());
        match result.expect("trailers read once") {
            Ok(trailers) => drop(trailers),
            Err(error) => operation.error = Some(error_case(&error)),
        }
        if end.failed {
            operation.error = Some(25);
        }
        unsafe { sys::finish(operation) };
    } else if operation.wait.is_none() {
        operation.wait = Some(end.future.as_ref().unwrap().subscribe());
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_http_drop_readable(handle: u32) {
    let operation = unsafe { sys::op(handle) };
    drop(operation.wait.take());
    let input = unsafe { sys::payload::<Input>(operation) };
    drop(input.stream.take());
    let end = unsafe { sys::op(input.end) };
    if !end.notified {
        let future = IncomingBody::finish(input.body.take().unwrap());
        let replacement = Operation::new(0, 0).with(
            End {
                future: Some(future),
                failed: input.failed,
            },
            Some(end_step),
        );
        end.payload = replacement.payload;
        end.destroy = replacement.destroy;
        end.step = replacement.step;
        core::mem::forget(replacement);
    } else {
        drop(input.body.take());
    }
    drop(input.response.take());
    unsafe {
        sys::p2_drop(input.end);
        sys::p2_drop(handle);
    }
}
