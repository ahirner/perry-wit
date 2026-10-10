//! `ReadableStream<Uint8Array>`: over a WIT `stream<u8>` (an export argument or a fetched
//! body) or over a body snapshot, with default and BYOB readers.
//!
//! A default reader has one outstanding read; BYOB readers queue transferred views. A copy
//! completes inline or by a stream event
//! ([web_event]); an empty copy of an open stream reads again. When the writer drops its end
//! the stream ends; a stream with a pending completion (a fetched body whose trailers decide
//! between success and truncation) holds its final read until [complete] reports the outcome.
use crate::*;

mod byob;

/// Canonical built-ins for `stream<u8>`. `src/link` binds these placeholders to the built-ins
/// of a function of the WIT world that carries a `stream<u8>` (all such streams share one
/// type per component instance).
mod canon {
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "perry:canon")]
    unsafe extern "C" {
        #[link_name = "[async-lower][stream-read]u8"]
        pub fn read(stream: u32, buffer: *mut u8, len: u32) -> u32;
        #[link_name = "[stream-cancel-read]u8"]
        pub fn cancel_read(stream: u32) -> u32;
        #[link_name = "[stream-drop-readable]u8"]
        pub fn drop_readable(stream: u32);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::missing_safety_doc)]
    mod host {
        pub unsafe fn read(_: u32, _: *mut u8, _: u32) -> u32 {
            unreachable!("streams exist in linked modules")
        }
        pub unsafe fn cancel_read(_: u32) -> u32 {
            unreachable!("streams exist in linked modules")
        }
        pub unsafe fn drop_readable(_: u32) {}
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub use host::*;
}

/// Default-reader transfer size. BYOB reads use the destination's remaining capacity.
const READ_SIZE: u32 = 8192;

const OPEN: u32 = 0;
const ENDED: u32 = 1;

/// Outcome of a stream whose end is confirmed separately (fetched bodies).
pub(crate) const COMPLETION_NONE: u32 = 0;
pub(crate) const COMPLETION_PENDING: u32 = 1;
const COMPLETION_FAILED: u32 = 2;

/// A `ReadableStream<Uint8Array>`.
#[repr(C)]
pub(crate) struct Readable {
    pin: u32,
    /// The readable end of a `stream<u8>`, 0 once dropped (or for a snapshot).
    handle: u32,
    state: u32,
    locked: u32,
    /// Set by the first read or cancel (`bodyUsed`).
    pub(crate) disturbed: u32,
    /// A body snapshot delivered as one chunk, if not null.
    memory: *mut u8,
    memory_len: u32,
    completion: u32,
    /// The reader whose final read waits for the completion.
    parked: *mut Reader,
    task: *mut u8,
    next: *mut Readable,
}

/// A default or BYOB reader, or the internal reader of `bytes()`/`text()`.
#[repr(C)]
pub(crate) struct Reader {
    pin: u32,
    stream: *mut Readable,
    released: u32,
    /// The outstanding read's promise and whether it is leased.
    pending: *mut u8,
    pending_leased: u32,
    reads: *mut byob::Read,
    tail: *mut byob::Read,
    /// 1 while the host fills `buffer`.
    copying: u32,
    buffer: *mut u8,
    /// The previous read's promise, result and view, reused by leased reads.
    promise: *mut u8,
    result: *mut ReadResult,
    view: *mut u8,
    /// The buffer of leased reads, allocated once.
    leased: *mut u8,
    /// Collects the whole body for `bytes()`/`text()`.
    drain: *mut Drain,
    next: *mut Reader,
}

/// `{ done, value }`.
#[repr(C)]
struct ReadResult {
    done: u32,
    value: *mut u8,
}

/// A body read to its end: the promise of `bytes()`, `text()` or `json()`.
struct Drain {
    pin: u32,
    promise: *mut u8,
    form: Form,
    contents: Buffer,
    /// The next `json()` drain waiting to be parsed.
    next: *mut Drain,
}

struct Lists {
    readables: *mut Readable,
    waiting: *mut Reader,
    /// `json()` drains that read their whole body, for [take_parsed].
    parsing: *mut Drain,
}
struct Global(core::cell::UnsafeCell<Lists>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Global {}
static LISTS: Global = Global(core::cell::UnsafeCell::new(Lists {
    readables: core::ptr::null_mut(),
    waiting: core::ptr::null_mut(),
    parsing: core::ptr::null_mut(),
}));

fn lists() -> &'static mut Lists {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *LISTS.0.get() }
}

fn new_readable(handle: u32, memory: *mut u8, memory_len: u32, completion: u32) -> *mut Readable {
    let stream = perry_abi::object!(Readable; memory, parked);
    // SAFETY: a fresh block, linked into the list of open streams.
    unsafe {
        stream.write(Readable {
            pin: if handle != 0 {
                perry_abi::pin(stream.cast())
            } else {
                0
            },
            handle,
            state: OPEN,
            locked: 0,
            disturbed: 0,
            memory,
            memory_len,
            completion,
            parked: core::ptr::null_mut(),
            task: perry_abi::task(),
            next: lists().readables,
        })
    };
    if handle != 0 {
        lists().readables = stream;
    }
    stream
}

/// A stream over the readable end `handle` (an export's `stream<u8>` argument), owned by the
/// current task: [web_streams_close] drops it at the task's end if the program did not.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_new(handle: u32) -> *mut u8 {
    new_readable(handle, core::ptr::null_mut(), 0, COMPLETION_NONE) as *mut u8
}

/// A fetched body: its end counts once [complete] confirms it.
pub(crate) fn body_stream(handle: u32) -> *mut Readable {
    new_readable(handle, core::ptr::null_mut(), 0, COMPLETION_PENDING)
}

/// A stream delivering `len` bytes at `data` (a body snapshot) as one chunk.
pub(crate) fn snapshot_stream(data: *mut u8, len: u32) -> *mut Readable {
    let stream = new_readable(0, data, len, COMPLETION_NONE);
    if len == 0 {
        // SAFETY: just created.
        unsafe { (*stream).memory = core::ptr::null_mut() };
    }
    stream
}

fn readable<'a>(stream: *mut u8) -> &'a mut Readable {
    // SAFETY: compiled code passes readable streams.
    unsafe { &mut *(stream as *mut Readable) }
}

fn reader<'a>(r: *mut u8) -> &'a mut Reader {
    // SAFETY: compiled code passes readers.
    unsafe { &mut *(r as *mut Reader) }
}

/// Drop the readable end and forget the stream.
fn end(stream: &mut Readable) {
    perry_abi::unpin(core::mem::take(&mut stream.pin));
    stream.state = ENDED;
    stream.memory = core::ptr::null_mut();
    if stream.handle != 0 {
        perry_abi::unjoin(stream.handle);
        // SAFETY: the readable end this stream owns.
        unsafe { canon::drop_readable(stream.handle) };
        stream.handle = 0;
    }
    let this = stream as *mut Readable;
    let mut link: *mut *mut Readable = &mut lists().readables;
    // SAFETY: the list holds live streams.
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

/// `stream.locked`.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_locked(stream: *mut u8) -> u32 {
    readable(stream).locked
}

fn new_reader(stream: *mut Readable) -> *mut Reader {
    let r = perry_abi::object!(Reader; stream, pending, buffer, promise, result, view, leased, drain, reads, tail);
    // SAFETY: a fresh block; the caller locked the stream.
    unsafe {
        (*stream).locked = 1;
        r.write(Reader {
            pin: 0,
            stream,
            released: 0,
            pending: core::ptr::null_mut(),
            pending_leased: 0,
            reads: core::ptr::null_mut(),
            tail: core::ptr::null_mut(),
            copying: 0,
            buffer: core::ptr::null_mut(),
            promise: core::ptr::null_mut(),
            result: core::ptr::null_mut(),
            view: core::ptr::null_mut(),
            leased: core::ptr::null_mut(),
            drain: core::ptr::null_mut(),
            next: core::ptr::null_mut(),
        })
    };
    r
}

/// `stream.getReader()`; `TypeError` while another reader holds the lock.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_get_reader(stream: *mut u8) -> *mut u8 {
    if readable(stream).locked != 0 {
        perry_abi::raise(TYPE_ERROR, "Invalid state: ReadableStream is locked");
        return core::ptr::null_mut();
    }
    new_reader(stream as *mut Readable) as *mut u8
}

/// `stream.cancel()` of an unlocked stream.
#[unsafe(no_mangle)]
pub extern "C" fn web_readable_cancel(stream: *mut u8) -> *mut u8 {
    let promise = promise_new();
    let source = readable(stream);
    if source.locked != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: ReadableStream is locked"),
        );
    } else {
        source.disturbed = 1;
        end(source);
        resolve(promise, perry_abi::UNDEFINED_BITS);
    }
    promise
}

/// Settle the user's read with `{ done, value }`; leased reads reuse the reader's objects.
fn settle_read(r: &mut Reader, promise: *mut u8, leased: bool, data: *mut u8, count: u32) {
    let (result, view) = if leased && !r.result.is_null() {
        (r.result, r.view)
    } else {
        (perry_abi::object!(ReadResult; value), core::ptr::null_mut())
    };
    let value = if count == 0 {
        core::ptr::null_mut()
    } else if view.is_null() {
        bytes_view(count, data)
    } else {
        // SAFETY: the reader's own view header.
        unsafe {
            *(view as *mut u32) = count;
            let view = &mut *(view as *mut perry_abi::Bytes);
            view.data = data;
            (*view.owner).data = data;
            (*view.owner).len = count;
            view.offset = 0;
        }
        view
    };
    // SAFETY: a result object of this reader or a fresh one.
    unsafe {
        result.write(ReadResult {
            done: u32::from(count == 0),
            value,
        })
    };
    if leased {
        r.result = result;
        if !value.is_null() {
            r.view = value;
        }
    }
    resolve(promise, result as usize as u64);
}

/// Deliver `count` bytes at `data` (0: the end) to the reader's consumer. Returns true if a
/// drain wants the next chunk.
fn deliver(r: &mut Reader, data: *mut u8, count: u32) -> bool {
    if !r.reads.is_null() {
        return byob::deliver(r, count);
    }
    if !r.drain.is_null() {
        // SAFETY: drains live as long as their reader.
        let drain = unsafe { &mut *r.drain };
        if count > 0 {
            // SAFETY: `count` bytes were copied to `data`.
            drain
                .contents
                .extend_from_slice(unsafe { slice(data as u32, count) });
            return true;
        }
        if drain.form == Form::Json {
            drain.pin = perry_abi::pin((drain as *mut Drain).cast());
            drain.next = lists().parsing;
            lists().parsing = drain;
        } else {
            settle_body(drain.promise, drain.form, drain.contents.as_slice());
        }
        return false;
    }
    let promise = core::mem::replace(&mut r.pending, core::ptr::null_mut());
    settle_read(r, promise, r.pending_leased != 0, data, count);
    false
}

/// The stream ended: deliver the end, or wait for the completion.
fn at_end(r: &mut Reader) {
    // SAFETY: readers point at their stream.
    let stream = unsafe { &mut *r.stream };
    match stream.completion {
        COMPLETION_PENDING => stream.parked = r,
        COMPLETION_FAILED => fail(r),
        _ => while deliver(r, core::ptr::null_mut(), 0) {},
    }
}

/// A truncated body: the read (or drain) rejects as Node's `fetch` does.
fn fail(r: &mut Reader) {
    if !r.reads.is_null() {
        return byob::reject(r, "terminated");
    }
    let promise = if r.drain.is_null() {
        core::mem::replace(&mut r.pending, core::ptr::null_mut())
    } else {
        // SAFETY: drains live as long as their reader.
        unsafe { (*r.drain).promise }
    };
    reject_with(promise, TYPE_ERROR, from_static("terminated"));
}

/// Report whether a stream with a pending completion ended intact; a read waiting for it
/// settles now.
pub(crate) fn complete(stream: *mut Readable, ok: bool) {
    // SAFETY: fetches keep their body streams.
    let stream = unsafe { &mut *stream };
    stream.completion = if ok {
        COMPLETION_NONE
    } else {
        COMPLETION_FAILED
    };
    let parked = core::mem::replace(&mut stream.parked, core::ptr::null_mut());
    if !parked.is_null() {
        // SAFETY: a parked reader is live.
        at_end(unsafe { &mut *parked });
    }
}

/// Read until the reader waits on the host or its consumer is served.
fn pump(r: &mut Reader) {
    loop {
        if !r.reads.is_null() && byob::short_closed_queue(r) {
            end(unsafe { &mut *r.stream });
            unsafe {
                (*r.stream).completion = COMPLETION_FAILED;
            }
            return byob::reject(r, "The closed stream cannot fill the requested minimum");
        }
        // SAFETY: readers retain their stream; helpers do not collect internally.
        let stream = unsafe { &mut *r.stream };
        stream.disturbed = 1;
        if !stream.memory.is_null() {
            if !r.reads.is_null() {
                let (destination, capacity) = byob::destination(r);
                let count = capacity.min(stream.memory_len);
                unsafe {
                    core::ptr::copy(stream.memory, destination, count as usize);
                }
                stream.memory_len -= count;
                stream.memory = if stream.memory_len == 0 {
                    core::ptr::null_mut()
                } else {
                    unsafe { stream.memory.add(count as usize) }
                };
                if stream.handle == 0 {
                    stream.state = ENDED;
                }
                if deliver(r, destination, count) {
                    continue;
                }
            } else {
                let (data, len) = (stream.memory, stream.memory_len);
                stream.memory = core::ptr::null_mut();
                if stream.handle == 0 {
                    stream.state = ENDED;
                }
                if deliver(r, data, len) {
                    continue;
                }
            }
            return;
        }
        if stream.state != OPEN {
            return at_end(r);
        }
        let capacity = if !r.reads.is_null() {
            let (data, capacity) = byob::destination(r);
            r.buffer = data;
            capacity
        } else {
            r.buffer = if r.pending_leased != 0 || !r.drain.is_null() {
                if r.leased.is_null() {
                    r.leased = alloc(READ_SIZE);
                }
                r.leased
            } else {
                alloc(READ_SIZE)
            };
            READ_SIZE
        };
        // SAFETY: the reader retains the buffer until completion or cancellation acknowledges it.
        let code = unsafe { canon::read(stream.handle, r.buffer, capacity) };
        if code == perry_abi::BLOCKED {
            perry_abi::join(stream.handle);
            r.copying = 1;
            r.pin = perry_abi::pin((r as *mut Reader).cast());
            r.next = lists().waiting;
            lists().waiting = r;
            return;
        }
        if !copied(r, code) {
            return;
        }
    }
}

/// A copy finished with `code`. Returns true if the reader should read again.
fn copied(r: &mut Reader, code: u32) -> bool {
    let count = code >> 4;
    // SAFETY: readers point at their stream.
    let stream = unsafe { &mut *r.stream };
    if code & 0xF != COPY_COMPLETED {
        // The writer dropped its end (or the read was cancelled): no more bytes.
        end(stream);
    }
    if count == 0 {
        // An empty copy of an open stream reads again; an ended stream delivers its end.
        if stream.state == OPEN {
            return true;
        }
        at_end(r);
        return false;
    }
    deliver(r, r.buffer, count)
}

const COPY_COMPLETED: u32 = 0;

/// `await reader.read()`. A leased read (the compiler proved that nothing from the previous
/// read is live) reuses the reader's buffer, promise and result; others get fresh ones.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_read(r: *mut u8, leased: u32) -> *mut u8 {
    let r = reader(r);
    let leased = leased != 0;
    let promise = if leased {
        perry_abi::promise_reuse(r.promise)
    } else {
        promise_new()
    };
    if leased {
        r.promise = promise;
    }
    if r.released != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: The reader is not attached to a stream"),
        );
        return promise;
    }
    if !r.pending.is_null() {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Concurrent reads of one reader are not supported"),
        );
        return promise;
    }
    r.pending = promise;
    r.pending_leased = u32::from(leased);
    pump(r);
    promise
}

/// Unlink `r` from the readers waiting on the host.
fn unwait(r: *mut Reader) {
    let mut link: *mut *mut Reader = &mut lists().waiting;
    // SAFETY: the list holds live readers.
    unsafe {
        perry_abi::unpin(core::mem::take(&mut (*r).pin));
        (*r).copying = 0;
        while !(*link).is_null() {
            if *link == r {
                *link = (*r).next;
                break;
            }
            link = &mut (**link).next;
        }
    }
}

/// A callback event of the current task: complete the copy waiting on `handle`. Returns 1 if
/// the event was one of a stream read.
#[unsafe(no_mangle)]
pub extern "C" fn web_event(event: u32, handle: u32, code: u32) -> u32 {
    if event != perry_abi::EVENT_STREAM_READ {
        return 0;
    }
    let mut r = lists().waiting;
    // SAFETY: waiting readers are live.
    unsafe {
        while !r.is_null() {
            if (*(*r).stream).handle == handle {
                let reader = &mut *r;
                unwait(r);
                if copied(reader, code) {
                    pump(reader);
                }
                return 1;
            }
            r = (*r).next;
        }
    }
    0
}

/// Stop the host copy of `r`, if any; what was copied is delivered.
fn cancel_copy(r: &mut Reader) {
    if r.copying == 0 {
        return;
    }
    let code = stop_copy(r);
    copied(r, code);
}

/// Detach the waitable, acknowledge cancellation, then release its buffer pin.
fn stop_copy(r: &mut Reader) -> u32 {
    // SAFETY: the reader retains its stream and buffer until cancellation acknowledges it.
    let handle = unsafe { (*r.stream).handle };
    perry_abi::unjoin(handle);
    let code = perry_abi::acknowledged(|| unsafe { canon::cancel_read(handle) });
    unwait(r);
    code
}

/// `reader.cancel()`: ends the stream; an outstanding read settles first.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_cancel(r: *mut u8) -> *mut u8 {
    let r = reader(r);
    let promise = promise_new();
    if r.released != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: The reader is not attached to a stream"),
        );
        return promise;
    }
    if !r.reads.is_null() {
        if r.copying != 0 {
            stop_copy(r);
        }
        byob::cancel(r);
    } else {
        cancel_copy(r);
    }
    // SAFETY: readers point at their stream.
    let stream = unsafe { &mut *r.stream };
    stream.disturbed = 1;
    stream.completion = COMPLETION_NONE;
    stream.parked = core::ptr::null_mut();
    end(stream);
    if !r.pending.is_null() {
        deliver(r, core::ptr::null_mut(), 0);
    }
    resolve(promise, perry_abi::UNDEFINED_BITS);
    promise
}

/// `reader.releaseLock()`: an outstanding read rejects; the stream stays open for a new
/// reader.
#[unsafe(no_mangle)]
pub extern "C" fn web_reader_release_lock(r: *mut u8) {
    let r = reader(r);
    if r.released != 0 {
        return;
    }
    let copied = if r.copying != 0 { stop_copy(r) >> 4 } else { 0 };
    if !r.reads.is_null() {
        byob::requeue(r, copied);
        byob::reject(r, "Invalid state: Releasing reader");
    }
    // SAFETY: readers point at their stream.
    let stream = unsafe { &mut *r.stream };
    if core::ptr::eq(stream.parked, r) {
        stream.parked = core::ptr::null_mut();
    }
    if !r.pending.is_null() {
        let promise = core::mem::replace(&mut r.pending, core::ptr::null_mut());
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Invalid state: Releasing reader"),
        );
    }
    r.released = 1;
    stream.locked = 0;
}

/// `result.done`.
#[unsafe(no_mangle)]
pub extern "C" fn web_read_done(result: *mut u8) -> u32 {
    // SAFETY: compiled code passes read results.
    unsafe { (*(result as *const ReadResult)).done }
}

/// `result.value`: bytes, or null (`undefined`) after default-reader EOF or BYOB cancellation.
#[unsafe(no_mangle)]
pub extern "C" fn web_read_value(result: *mut u8) -> *mut u8 {
    // SAFETY: compiled code passes read results.
    unsafe { (*(result as *const ReadResult)).value }
}

/// `bytes()`/`text()`/`json()` of a body stream: read it to the end through an internal
/// reader.
pub(crate) fn drain(stream: *mut Readable, form: Form, promise: *mut u8) {
    // SAFETY: bodies keep their streams.
    if unsafe { (*stream).locked } != 0 {
        reject_with(
            promise,
            TYPE_ERROR,
            from_static("Body is unusable: Body has already been read"),
        );
        return;
    }
    let r = new_reader(stream);
    let drain = perry_abi::object!(Drain; promise, contents);
    // SAFETY: fresh blocks.
    unsafe {
        drain.write(Drain {
            pin: 0,
            promise,
            form,
            contents: Buffer::new(),
            next: core::ptr::null_mut(),
        });
        (*r).drain = drain;
        pump(&mut *r);
    }
}

/// What a body is read as.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Form {
    Bytes,
    Text,
    Json,
}

/// Settle `promise` with a whole body read as bytes or text. The contents belong to the
/// read. (`json()` bodies are parsed by [crate::body::web_json_settle], so that programs
/// without `json()` do not link the JSON parser.)
pub(crate) fn settle_body(promise: *mut u8, form: Form, contents: &[u8]) {
    let value = if form == Form::Text {
        decode(contents)
    } else {
        bytes_view(contents.len() as u32, contents.as_ptr() as *mut u8)
    };
    resolve(promise, value as usize as u64);
}

/// Take the `json()` drains that read their whole body: `(promise, contents)` each.
pub(crate) fn take_parsed(mut each: impl FnMut(*mut u8, &[u8])) {
    while let Some(drain) = core::ptr::NonNull::new(lists().parsing) {
        // SAFETY: listed drains are live.
        let drain = unsafe { &mut *drain.as_ptr() };
        lists().parsing = drain.next;
        each(drain.promise, drain.contents.as_slice());
        perry_abi::unpin(core::mem::take(&mut drain.pin));
    }
}

/// The text of a body: UTF-8 with the BOM removed and invalid bytes replaced.
pub(crate) fn decode(contents: &[u8]) -> *mut u8 {
    let contents = contents.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(contents);
    let mut text = Buffer::new();
    for chunk in contents.utf8_chunks() {
        text.extend_from_slice(chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            text.extend_from_slice("\u{FFFD}".as_bytes());
        }
    }
    from_bytes(text.as_slice())
}

/// Whether a fetched body's final read still awaits its completion acknowledgement.
pub(crate) fn pending_completion(stream: *mut Readable) -> bool {
    // SAFETY: fetch operations retain their body, or have not received one yet.
    !stream.is_null() && unsafe { !(*stream).parked.is_null() }
}

/// Outstanding read promises owned by this task. Open, unread streams are not work.
#[unsafe(no_mangle)]
pub extern "C" fn web_streams_pending() -> u32 {
    let task = perry_abi::task();
    let mut reader = lists().waiting;
    let mut pending = 0;
    // SAFETY: waiting readers and their streams are pinned until acknowledgement.
    unsafe {
        while !reader.is_null() {
            pending += u32::from((*(*reader).stream).task == task);
            reader = (*reader).next;
        }
    }
    pending
}

/// At the end of the current task: cancel its outstanding copies and drop the streams it
/// still owns, so no host handle outlives the export call.
#[unsafe(no_mangle)]
pub extern "C" fn web_streams_close() {
    let task = perry_abi::task();
    let mut r = lists().waiting;
    // SAFETY: listed readers and streams are live.
    unsafe {
        while !r.is_null() {
            if (*(*r).stream).task == task {
                stop_copy(&mut *r);
                r = lists().waiting;
            } else {
                r = (*r).next;
            }
        }
        let mut stream = lists().readables;
        while !stream.is_null() {
            let next = (*stream).next;
            if (*stream).task == task {
                end(&mut *stream);
            }
            stream = next;
        }
    }
}
