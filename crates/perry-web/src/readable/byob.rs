//! Transferred views and queued BYOB reads. Filling and cancellation follow main's stream
//! contract; collectible request nodes retain only active promises, views and successors.
use super::*;
use core::ptr;

#[repr(C)]
pub(super) struct Read {
    promise: *mut u8,
    view: *mut perry_abi::Bytes,
    minimum: u32,
    written: u32,
    next: *mut Read,
}

/// `reader.read(view, {min})`: validate first, then transfer the complete buffer.
#[unsafe(no_mangle)]
pub(super) extern "C" fn web_reader_read_byob(r: *mut u8, view: *mut u8, minimum: f64) -> *mut u8 {
    let r = reader(r);
    let promise = promise_new();
    let reject = |kind, message| {
        reject_with(promise, kind, from_static(message));
        promise
    };
    // SAFETY: compiled calls carry a live byte view and a reader.
    let bytes = unsafe { &*view.cast::<perry_abi::Bytes>() };
    if unsafe { bytes.detached() } || bytes.len == 0 {
        return reject(TYPE_ERROR, "view must have non-zero byteLength");
    }
    if !(1.0..18446744073709551616.0).contains(&minimum) {
        return reject(TYPE_ERROR, "options.min must be a positive integer");
    }
    let minimum = minimum as u64;
    if minimum > u64::from(bytes.len) {
        return reject(RANGE_ERROR, "options.min exceeds the view's length");
    }
    if r.released != 0 {
        return reject(
            TYPE_ERROR,
            "Invalid state: The reader is not attached to a stream",
        );
    }
    if unsafe { (*r.stream).completion == COMPLETION_FAILED } {
        return reject(TYPE_ERROR, "terminated");
    }
    let request = perry_abi::object!(Read; promise, view, next);
    unsafe {
        request.write(Read {
            promise,
            view: perry_abi::bytes_transfer(view).cast(),
            minimum: minimum as u32,
            written: 0,
            next: ptr::null_mut(),
        });
        if r.reads.is_null() {
            r.reads = request;
            r.tail = request;
            r.pending = promise;
            r.pending_leased = 0;
            pump(r);
        } else {
            (*r.tail).next = request;
            r.tail = request;
        }
    }
    promise
}

pub(super) fn destination(r: &mut Reader) -> (*mut u8, u32) {
    unsafe {
        let request = &*r.reads;
        let view = &*request.view;
        (
            view.data.add(request.written as usize),
            view.len - request.written,
        )
    }
}

/// Finish the first request, exposing either its transferred view or cancellation's `undefined`.
fn finish(r: &mut Reader, done: bool, cancelled: bool) {
    unsafe {
        let request = &mut *r.reads;
        let result = perry_abi::object!(ReadResult; value);
        (*request.view).len = request.written;
        result.write(ReadResult {
            done: u32::from(done),
            value: if cancelled {
                ptr::null_mut()
            } else {
                request.view.cast()
            },
        });
        resolve(request.promise, result as usize as u64);
        r.reads = request.next;
        request.next = ptr::null_mut();
        request.view = ptr::null_mut();
        request.promise = ptr::null_mut();
        r.pending = if r.reads.is_null() {
            r.tail = ptr::null_mut();
            ptr::null_mut()
        } else {
            (*r.reads).promise
        };
        r.buffer = ptr::null_mut();
    }
}

/// Add a completed transfer; return whether filling or the next queued read should continue.
pub(super) fn deliver(r: &mut Reader, count: u32) -> bool {
    unsafe {
        let request = &mut *r.reads;
        request.written += count;
        if count == 0 || request.written >= request.minimum {
            finish(r, count == 0, false);
        }
    }
    !r.reads.is_null()
}

pub(super) fn cancel(r: &mut Reader) {
    while !r.reads.is_null() {
        finish(r, true, true);
    }
}

pub(super) fn reject(r: &mut Reader, message: &'static str) {
    unsafe {
        while !r.reads.is_null() {
            let request = &mut *r.reads;
            reject_with(request.promise, TYPE_ERROR, from_static(message));
            r.reads = request.next;
            request.next = ptr::null_mut();
            request.view = ptr::null_mut();
            request.promise = ptr::null_mut();
        }
    }
    r.tail = ptr::null_mut();
    r.pending = ptr::null_mut();
    r.buffer = ptr::null_mut();
}

/// A known closed queue shorter than min is an error, as on main. A short host EOF discovered
/// while filling a request instead returns the bytes with `done: true`.
pub(super) fn short_closed_queue(r: &Reader) -> bool {
    unsafe {
        let stream = &*r.stream;
        let request = &*r.reads;
        stream.state == ENDED
            && !stream.memory.is_null()
            && request.written == 0
            && stream.memory_len < request.minimum
    }
}

/// Preserve bytes copied before releaseLock, including the cancellation acknowledgement's tail.
pub(super) fn requeue(r: &mut Reader, copied: u32) {
    unsafe {
        let request = &*r.reads;
        let count = request.written + copied;
        if count != 0 {
            let stream = &mut *r.stream;
            stream.memory = (*request.view).data;
            stream.memory_len = count;
        }
    }
}
