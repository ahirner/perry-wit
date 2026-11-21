//! P2 input streams and trailers for the shared response-body reader.
#![no_std]
use core::ptr;
use perry_p2_sys::{self as sys, BLOCKED, DROPPED, EVENT_STREAM_READ, Operation};
use wasip2::http::types::{ErrorCode, FutureTrailers, IncomingBody, IncomingResponse};
use wasip2::io::streams::{InputStream, StreamError};

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
    let end = sys::completion(40);
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
