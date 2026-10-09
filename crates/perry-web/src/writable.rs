//! Web writers over the console outputs: `Writable.toWeb(process.stdout)`.
use crate::*;

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
