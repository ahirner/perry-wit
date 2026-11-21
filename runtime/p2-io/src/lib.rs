//! P2 stdout/stderr streams behind the runtime's shared output protocol.
#![no_std]
use perry_p2_sys::{self as sys, BLOCKED, DROPPED, EVENT_STREAM_WRITE, Operation, wasip2};
use wasip2::io::streams::OutputStream;

struct Output {
    stream: Option<OutputStream>,
    completion: u32,
    pending: Option<(*const u8, u32)>,
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_io_new() -> u64 {
    let id = Operation::new(EVENT_STREAM_WRITE, 0)
        .with(
            Output {
                stream: None,
                completion: 0,
                pending: None,
            },
            Some(write_step),
        )
        .register();
    u64::from(id) | (u64::from(id) << 32)
}
fn start(id: u32, stderr: bool) -> u32 {
    let output = unsafe { sys::payload::<Output>(sys::op(id)) };
    output.stream = Some(if stderr {
        wasip2::cli::stderr::get_stderr()
    } else {
        wasip2::cli::stdout::get_stdout()
    });
    output.completion = sys::completion(8, false);
    output.completion
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_io_stdout(id: u32) -> u32 {
    start(id, false)
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_io_stderr(id: u32) -> u32 {
    start(id, true)
}

unsafe fn write(operation: &mut Operation, ptr: *const u8, len: u32) -> u32 {
    let output = unsafe { sys::payload::<Output>(operation) };
    let stream = output.stream.as_ref().unwrap();
    match stream.check_write() {
        Ok(0) => {
            output.pending = Some((ptr, len));
            operation.ready = false;
            operation.notified = false;
            if operation.wait.is_none() {
                operation.wait = Some(stream.subscribe());
            }
            BLOCKED
        }
        Ok(capacity) => {
            let n = u64::from(len).min(capacity) as u32;
            if stream.write(unsafe { sys::bytes(ptr, n) }).is_err() {
                DROPPED
            } else {
                n << 4
            }
        }
        Err(_) => DROPPED,
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_io_write(id: u32, ptr: *const u8, len: u32) -> u32 {
    unsafe { write(sys::op(id), ptr, len) }
}
unsafe extern "C" fn write_step(operation: *mut Operation) {
    let operation = unsafe { &mut *operation };
    let Some((ptr, len)) = (unsafe { sys::payload::<Output>(operation) }).pending else {
        return;
    };
    let code = unsafe { write(operation, ptr, len) };
    if code != BLOCKED {
        unsafe { sys::payload::<Output>(operation) }.pending = None;
        operation.code = code;
        unsafe { sys::finish(operation) };
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_io_drop_writer(id: u32) {
    let operation = unsafe { sys::op(id) };
    drop(operation.wait.take());
    let output = unsafe { sys::payload::<Output>(operation) };
    if let Some(stream) = output.stream.take() {
        let _ = stream.blocking_flush();
    }
    if output.completion != 0 {
        unsafe { sys::finish(sys::op(output.completion)) };
    }
    unsafe { sys::p2_drop(id) };
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn p2_io_cancel_write(id: u32) -> u32 {
    let operation = unsafe { sys::op(id) };
    unsafe { sys::payload::<Output>(operation) }.pending = None;
    operation.notified = true;
    drop(operation.wait.take());
    0
}
#[unsafe(no_mangle)]
pub(crate) extern "C" fn p2_io_flush() {
    unsafe {
        while sys::io_close() == 0 {
            let set = sys::task_suspend();
            let event = sys::p2_poll(set);
            sys::io_event(*event, *event.add(1), *event.add(2));
        }
    }
}
