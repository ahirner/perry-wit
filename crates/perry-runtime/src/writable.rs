//! Stdio Web writers share the console's ordered P3 transfer queue.
use crate::{UNDEFINED_NUMBER, bytes, exception, output, promise, region::current};
use core::ptr;

#[derive(Clone, Copy)]
enum State {
    Open,
    Closing(*mut u8),
    Closed,
    Failed(*mut u8),
}

pub(crate) struct Stream {
    stderr: bool,
    writer: *mut Writer,
    state: State,
    pending: u32,
}

struct Writer {
    stream: *mut Stream,
}

fn reject(promise: *mut u8, error: *mut u8) {
    // SAFETY: errors and promises belong to the current task.
    let status = unsafe { exception::perry_exception_raise(error) };
    promise::perry_promise_reject(promise, status);
}

fn invalid(message: &'static str) -> *mut u8 {
    let promise = promise::perry_promise_new();
    let status = exception::raise_error(exception::TYPE_ERROR, message);
    promise::perry_promise_reject(promise, status);
    promise
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_writable_new(stderr: u32) -> *mut Stream {
    let stream = current().allocate(size_of::<Stream>()).cast::<Stream>();
    // SAFETY: freshly allocated, task-owned storage.
    unsafe {
        stream.write(Stream {
            stderr: stderr != 0,
            writer: ptr::null_mut(),
            state: State::Open,
            pending: 0,
        });
    }
    stream
}

/// # Safety
/// `stream` is a live stream owned by the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_writable_locked(stream: *mut Stream) -> u32 {
    // SAFETY: guaranteed by the caller.
    u32::from(unsafe { !(*stream).writer.is_null() })
}

/// # Safety
/// `stream` is a live stream owned by the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_writable_get_writer(stream: *mut Stream) -> *mut u8 {
    // SAFETY: caller guarantees the stream; the writer is freshly allocated.
    unsafe {
        if !(*stream).writer.is_null() {
            exception::raise_error(exception::TYPE_ERROR, "stream is locked");
            return ptr::null_mut();
        }
        let writer = current().allocate(size_of::<Writer>()).cast::<Writer>();
        writer.write(Writer { stream });
        (*stream).writer = writer;
        writer.cast()
    }
}

/// # Safety
/// `writer` is a live writer owned by the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_writer_release(writer: *mut u8) {
    // SAFETY: guaranteed by the caller, including released writers.
    unsafe {
        let writer = writer.cast::<Writer>();
        let stream = &mut *(*writer).stream;
        if stream.writer == writer {
            stream.writer = ptr::null_mut();
        }
    }
}

/// # Safety
/// `writer` and `view` are live task-owned values. Bytes remain live until acknowledgement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_writer_write(writer: *mut u8, view: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller; the output queue borrows task-owned bytes.
    unsafe {
        let writer = writer.cast::<Writer>();
        let stream = &mut *(*writer).stream;
        if stream.writer != writer {
            return invalid("writer has released its lock");
        }
        match stream.state {
            State::Open => {}
            State::Failed(error) => {
                let promise = promise::perry_promise_new();
                reject(promise, error);
                return promise;
            }
            _ => return invalid("stream is closing or closed"),
        }
        let promise = promise::perry_promise_new();
        let data = core::slice::from_raw_parts(
            bytes::perry_bytes_data(view),
            bytes::perry_bytes_length(view) as usize,
        );
        stream.pending += 1;
        let destination = output::Destination::Writer { stream, promise };
        if stream.stderr {
            output::stderr::enqueue(data, destination);
        } else {
            output::stdout::enqueue(data, destination);
        }
        promise
    }
}

/// # Safety
/// `writer` is a live task-owned writer. Closing the wrapper leaves the host channel usable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_writer_close(writer: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let writer = writer.cast::<Writer>();
        let stream = &mut *(*writer).stream;
        if stream.writer != writer {
            return invalid("writer has released its lock");
        }
        let promise = promise::perry_promise_new();
        match stream.state {
            State::Open => {
                if stream.pending == 0 {
                    stream.state = State::Closed;
                    promise::perry_promise_resolve(promise, UNDEFINED_NUMBER);
                } else {
                    stream.state = State::Closing(promise);
                }
            }
            State::Failed(error) => reject(promise, error),
            _ => {
                let status =
                    exception::raise_error(exception::TYPE_ERROR, "stream is closing or closed");
                promise::perry_promise_reject(promise, status);
            }
        }
        promise
    }
}

impl Stream {
    pub(crate) fn failed(&self) -> bool {
        matches!(self.state, State::Failed(_))
    }

    pub(crate) fn fail(&mut self) {
        if self.failed() {
            return;
        }
        let status = exception::raise_error(exception::ERROR, "output transfer failed");
        let error = exception::perry_exception_catch(status);
        if let State::Closing(promise) = self.state {
            reject(promise, error);
        }
        self.state = State::Failed(error);
    }

    pub(crate) fn complete(&mut self, promise: *mut u8) {
        self.pending -= 1;
        match self.state {
            State::Failed(error) => reject(promise, error),
            _ => promise::perry_promise_resolve(promise, UNDEFINED_NUMBER),
        }
        if self.pending == 0
            && let State::Closing(promise) = self.state
        {
            self.state = State::Closed;
            promise::perry_promise_resolve(promise, UNDEFINED_NUMBER);
        }
    }
}
