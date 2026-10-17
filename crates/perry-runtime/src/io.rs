//! Console output, process state, environment and clocks for WASI P3 components.
//!
//! `console.log` and `console.error` append to the current task's output buffers
//! ([perry_print]); nothing blocks there, as in Node. The buffers drain through one
//! `wasi:cli/stdout` (or `stderr`) stream per task, opened on first use: [perry_io_pump]
//! starts async `stream.write`s whenever the task is about to wait or finish, a blocked write
//! joins the task's waitable set and completes through [perry_io_event], and [perry_io_close]
//! drops the writable end and waits for the host's completion future before the export
//! returns. A buffer being written is never touched; new output goes to a second one.
//!
//! `process.exitCode` and `process.exit` are instance state ([perry_exit_code_set],
//! [perry_exit]); `process.exit` unwinds with status 5, which no handler catches.
use crate::{
    exception::{
        ERROR, EXIT, KIND, REFERENCE_ERROR, SYNTAX_ERROR, TAG, TAG_BOOLEAN, TAG_JSON, TAG_NULL,
        TAG_NUMBER, TAG_STRING, TAG_UNDEFINED, TYPE_ERROR, VALUE,
    },
    host::{self, BLOCKED, DROPPED, EVENT_FUTURE_READ, EVENT_STREAM_WRITE},
    instance::instance_state,
    region::current,
};
#[cfg(target_arch = "wasm32")]
use core::cell::UnsafeCell;

/// A growable byte buffer in the task region: released with the task, so a component that
/// prints never needs the buffer allocator's `free` path.
struct Bytes {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl Bytes {
    const fn new() -> Self {
        Bytes {
            ptr: core::ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    #[inline(never)]
    fn extend_from_slice(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > self.cap {
            let cap = (self.cap * 2).max(self.len + bytes.len()).max(64);
            let ptr = current().allocate(cap);
            if self.len > 0 {
                // SAFETY: both regions are live and disjoint.
                unsafe { core::ptr::copy_nonoverlapping(self.ptr, ptr, self.len) };
            }
            self.ptr = ptr;
            self.cap = cap;
        }
        // SAFETY: capacity was ensured above.
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len())
        };
        self.len += bytes.len();
    }

    fn push(&mut self, byte: u8) {
        self.extend_from_slice(&[byte]);
    }

    fn push_str(&mut self, text: &str) {
        self.extend_from_slice(text.as_bytes());
    }

    fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` bytes at `ptr` are initialized.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

/// One output channel of a task.
pub(crate) struct Output {
    writable: u32,
    future: u32,
    /// Bytes handed to `stream.write`; immutable while `blocked`.
    writing: Bytes,
    written: usize,
    blocked: bool,
    /// Output produced while `writing` drains.
    queued: Bytes,
    state: State,
    result: [u8; 8],
    /// Bytes appended and bytes the host accepted, since the task started.
    appended: u64,
    flushed: u64,
    /// Acknowledged writes waiting for `flushed` to reach their end, oldest first.
    acks: *mut Ack,
    acks_tail: *mut Ack,
    spare_acks: *mut Ack,
}

/// A write whose promise settles once the host accepted its last byte.
struct Ack {
    promise: *mut u8,
    end: u64,
    next: *mut Ack,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    Failed,
    /// Writable end dropped; waiting for the completion future.
    Closing,
    Closed,
}

impl Output {
    pub(crate) unsafe fn trace_roots(&self, roots: &mut alloc::vec::Vec<*mut u8>) {
        let mut ack = self.acks;
        // SAFETY: only active acknowledgements in this live task are visited.
        unsafe {
            while !ack.is_null() {
                roots.push((*ack).promise);
                ack = (*ack).next;
            }
        }
    }

    pub(crate) const fn new() -> Self {
        Output {
            writable: 0,
            future: 0,
            writing: Bytes::new(),
            written: 0,
            blocked: false,
            queued: Bytes::new(),
            state: State::Open,
            result: [0; 8],
            appended: 0,
            flushed: 0,
            acks: core::ptr::null_mut(),
            acks_tail: core::ptr::null_mut(),
            spare_acks: core::ptr::null_mut(),
        }
    }

    /// Settle the acknowledged writes the host has accepted completely.
    fn acknowledge(&mut self) {
        // SAFETY: ack entries are live allocations of this task.
        unsafe {
            while !self.acks.is_null() && (*self.acks).end <= self.flushed {
                let ack = self.acks;
                self.acks = (*ack).next;
                if self.acks.is_null() {
                    self.acks_tail = core::ptr::null_mut();
                }
                (*ack).next = self.spare_acks;
                self.spare_acks = ack;
                let promise = core::mem::replace(&mut (*ack).promise, core::ptr::null_mut());
                if self.state == State::Failed {
                    let message = crate::string::from_static("Output stream is closed");
                    let exception = crate::exception::perry_exception_new(
                        crate::exception::ERROR,
                        TAG_STRING,
                        message as usize as u64,
                    );
                    let status = crate::exception::perry_exception_raise(exception);
                    crate::promise::perry_promise_reject(promise, status as i32);
                } else {
                    crate::promise::perry_promise_resolve(promise, crate::UNDEFINED_BITS);
                }
            }
        }
    }

    fn idle(&self) -> bool {
        !self.blocked && self.written == self.writing.len() && self.queued.is_empty()
    }
}

/// Append a string (`string` layout of [crate::string]) and, if `newline`, a line feed to
/// output `fd` (0 stdout, 1 stderr) of the current task.
///
/// # Safety
/// `string` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_print(fd: u32, string: *mut u8, newline: u32) {
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { crate::string::bytes(string) };
    print_bytes(fd as usize, bytes, newline != 0);
}

/// Append `len` bytes at `data` to output `fd` (0 stdout, 1 stderr) of the current task:
/// writes of `Writable.toWeb(process.stdout)` writers.
///
/// # Safety
/// `data` must point to `len` readable bytes.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_write(fd: u32, data: *const u8, len: u32) {
    if len == 0 {
        return;
    }
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { core::slice::from_raw_parts(data, len as usize) };
    print_bytes(fd as usize, bytes, false);
}

#[inline(never)]
fn print_bytes(fd: usize, bytes: &[u8], newline: bool) {
    let output = &mut current().outputs[fd.min(1)];
    if output.state != State::Open {
        return;
    }
    if output.idle() && output.writing.cap > output.queued.cap {
        // Acknowledgement ended the host borrow; serial writes can reuse this buffer.
        core::mem::swap(&mut output.writing, &mut output.queued);
        output.queued.clear();
        output.written = 0;
    }
    output.queued.extend_from_slice(bytes);
    output.appended += bytes.len() as u64;
    if newline {
        output.queued.push(b'\n');
        output.appended += 1;
    }
}

/// `writer.write(bytes)` of a Web writer over output `fd`: append `len` bytes at `data` and
/// settle `promise` once the host accepted them. Writing starts at once, so a writer that
/// awaits each write keeps at most one chunk buffered (bounded memory for streams).
///
/// # Safety
/// `data` must point to `len` readable bytes; `promise` must be a pending promise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_write_acknowledged(
    fd: u32,
    data: *const u8,
    len: u32,
    promise: *mut u8,
) {
    let fd = (fd as usize).min(1);
    // SAFETY: guaranteed by the caller.
    unsafe { perry_write(fd as u32, data, len) };
    let task = current();
    let output = &mut task.outputs[fd];
    let ack = if output.spare_acks.is_null() {
        task.allocate(core::mem::size_of::<Ack>()) as *mut Ack
    } else {
        let ack = output.spare_acks;
        // SAFETY: spare entries are live allocations of this task.
        output.spare_acks = unsafe { (*ack).next };
        ack
    };
    let output = &mut current().outputs[fd];
    // SAFETY: `ack` is exclusively ours; the tail, if any, is live.
    unsafe {
        ack.write(Ack {
            promise,
            end: output.appended,
            next: core::ptr::null_mut(),
        });
        if output.acks_tail.is_null() {
            output.acks = ack;
        } else {
            (*output.acks_tail).next = ack;
        }
    }
    output.acks_tail = ack;
    if output.state != State::Open {
        output.flushed = output.appended;
    }
    pump(fd);
    current().outputs[fd].acknowledge();
}

#[inline(never)]
fn finish_copy(output: &mut Output, code: u32) {
    output.written += (code >> 4) as usize;
    output.flushed += u64::from(code >> 4);
    if code & 0xF == DROPPED {
        // The reader is gone: discard the rest, like writes to a closed pipe.
        output.written = output.writing.len();
        output.queued.clear();
        output.flushed = output.appended;
        output.state = State::Failed;
    }
    output.acknowledge();
}

#[inline(never)]
fn pump(fd: usize) {
    let output = &mut current().outputs[fd];
    loop {
        if output.blocked || output.state != State::Open {
            return;
        }
        if output.written == output.writing.len() {
            if output.queued.is_empty() {
                return;
            }
            // The drained buffer takes the next output: no allocation per swap.
            core::mem::swap(&mut output.writing, &mut output.queued);
            output.queued.clear();
            output.written = 0;
        }
        if output.writable == 0 {
            let ends = host::output::new(fd);
            let readable = ends as u32;
            output.writable = (ends >> 32) as u32;
            output.future = host::output::start(fd, readable);
        }
        let rest = &output.writing.as_slice()[output.written..];
        let code = host::output::write(fd, output.writable, rest.as_ptr(), rest.len() as u32);
        if code == BLOCKED {
            output.blocked = true;
            let writable = output.writable;
            perry_task_join(writable);
            return;
        }
        finish_copy(output, code);
    }
}

/// Start writing buffered output of the current task.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_io_pump() {
    pump(0);
    pump(1);
}

#[inline(never)]
fn event_fd(fd: usize, event: u32, handle: u32, code: u32) -> u32 {
    let output = &mut current().outputs[fd];
    if event == EVENT_STREAM_WRITE && output.writable == handle && output.blocked {
        output.blocked = false;
        finish_copy(output, code);
        pump(fd);
        return 1;
    }
    if event == EVENT_FUTURE_READ && output.future == handle && output.state == State::Closing {
        // SAFETY: canonical built-ins on handles this output owns.
        unsafe { host::waitable_join(handle, 0) };
        host::output::drop_future(fd, handle);
        output.state = State::Closed;
        return 1;
    }
    0
}

/// Handle a stream or future event of the current task's output. Returns 1 if it was one.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_io_event(event: u32, handle: u32, code: u32) -> u32 {
    if event_fd(0, event, handle, code) != 0 {
        1
    } else {
        event_fd(1, event, handle, code)
    }
}

#[inline(never)]
fn close_fd(fd: usize) -> u32 {
    pump(fd);
    let output = &mut current().outputs[fd];
    match output.state {
        State::Open if output.writable == 0 => {
            output.state = State::Closed;
            1
        }
        State::Open | State::Failed if output.idle() => {
            // SAFETY: canonical built-ins on handles this output owns.
            unsafe { host::waitable_join(output.writable, 0) };
            host::output::drop_writer(fd, output.writable);
            output.state = State::Closing;
            let code = host::output::read(fd, output.future, output.result.as_mut_ptr());
            if code == BLOCKED {
                let future = output.future;
                perry_task_join(future);
                0
            } else {
                host::output::drop_future(fd, output.future);
                output.state = State::Closed;
                1
            }
        }
        State::Closed => 1,
        _ => 0,
    }
}

/// Flush and close the current task's outputs. Returns 1 once everything reached the host;
/// otherwise the task waits on its set for the events that finish it.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_io_close() -> u32 {
    close_fd(0) & close_fd(1)
}

#[inline(never)]
fn cancel_fd(task: *mut crate::region::Task, fd: usize) {
    let output = &mut current().outputs[fd];
    if output.state == State::Closed {
        return;
    }
    if output.writable != 0 && output.state != State::Closing {
        // SAFETY: this task owns the waitable; cancellation may run other tasks.
        unsafe { host::waitable_join(output.writable, 0) };
        if output.blocked {
            host::output::cancel_write(fd, output.writable);
            // SAFETY: the cancelling task stays live through acknowledgement.
            unsafe { crate::region::perry_task_resume(task) };
            output.blocked = false;
        }
        host::output::drop_writer(fd, output.writable);
    }
    if output.future != 0 {
        if output.state == State::Closing {
            unsafe { host::waitable_join(output.future, 0) };
            host::output::cancel_read(fd, output.future);
            // SAFETY: the cancelling task stays live through acknowledgement.
            unsafe { crate::region::perry_task_resume(task) };
        }
        host::output::drop_future(fd, output.future);
    }
    output.writable = 0;
    output.future = 0;
    output.state = State::Closed;
}

/// Cancel output transfers before their task-owned buffers are released.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_io_cancel() {
    let task = crate::region::perry_task_current();
    cancel_fd(task, 0);
    cancel_fd(task, 1);
}

/// Join `waitable` to the current task's waitable set, creating the set on first use.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_join(waitable: u32) {
    let task = current();
    if task.set == 0 {
        // SAFETY: canonical built-in.
        task.set = unsafe { host::waitable_set_new() };
    }
    // SAFETY: canonical built-in on a handle of this task.
    unsafe { host::waitable_join(waitable, task.set) };
}

/// A host subtask event: settle its promise and drop it once it returned.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_subtask_event(handle: u32, state: u32) {
    if state == host::SUBTASK_RETURNED {
        crate::region::perry_subtask_done(handle);
        // SAFETY: canonical built-in on a returned subtask of this task.
        unsafe { host::subtask_drop(handle) };
    }
}

/// Drop the current task's waitable set, if it has one.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_task_close_set() {
    crate::promise::cancel_observers();
    let task = current();
    if task.set != 0 {
        // SAFETY: canonical built-in; nothing is joined any more.
        unsafe { host::waitable_set_drop(task.set) };
        task.set = 0;
    }
}

pub(crate) struct Process {
    exit_code: i32,
}

impl Process {
    pub(crate) const fn new() -> Self {
        Self { exit_code: 0 }
    }
}

#[cfg(target_arch = "wasm32")]
struct Global(UnsafeCell<Process>);
// SAFETY: one instance runs one thread.
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Global {}
instance_state!(PROCESS, process_state, process, Process, Process::new());

#[allow(clippy::mut_from_ref)]
fn process() -> &'static mut Process {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *process_state() }
}

fn exit_code(code: f64) -> i32 {
    if code.is_finite() { code as i32 } else { 0 }
}

/// `process.exitCode = code`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_exit_code_set(code: f64) {
    process().exit_code = exit_code(code);
}

/// Report unresolved detached work once, before command output is drained.
///
/// # Safety
/// `message` is a live UTF-8 runtime string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_task_detached(count: u32, message: *mut u8) {
    if count != 0 {
        // SAFETY: compiled code passes a static diagnostic string.
        print_bytes(1, unsafe { crate::string::bytes(message) }, true);
        current().outcome = Some(1);
    }
}

/// `process.exit(code)`: record the code and return the exit status that unwinds to the
/// export.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_exit(code: f64) -> i32 {
    process().exit_code = exit_code(code);
    EXIT as i32
}

/// The outcome of a command whose main promise settled: 0 for `ok`, 1 for `err`. An uncaught
/// exception is reported on stderr. Idempotent within the task.
///
/// # Safety
/// `promise` must be a settled promise of the current task.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_command_outcome(promise: *mut u8) -> u32 {
    let task = current();
    if let Some(outcome) = task.outcome {
        return outcome;
    }
    // SAFETY: guaranteed by the caller.
    let (state, status, value) = unsafe {
        (
            *(promise as *const u32),
            *(promise.add(crate::promise::STATUS as usize) as *const i32) as u32,
            *(promise.add(crate::promise::VALUE as usize) as *const u64),
        )
    };
    if state == crate::promise::REJECTED && status != EXIT {
        let mut text = Bytes::new();
        text.push_str("Uncaught ");
        // SAFETY: a rejection's value is its exception object or null.
        unsafe { describe(&mut text, status, value as usize as *mut u8) };
        print_bytes(1, text.as_slice(), true);
        process().exit_code = 1;
    }
    let outcome = u32::from(process().exit_code != 0);
    current().outcome = Some(outcome);
    outcome
}

/// Queue an uncaught library rejection once, without changing the command exit code.
///
/// # Safety
/// `promise` is rejected in the current task.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_report_rejection(promise: *mut u8) {
    if current().outcome.is_some() {
        return;
    }
    let mut text = Bytes::new();
    text.push_str("Uncaught ");
    unsafe {
        crate::promise::perry_promise_rethrow(promise);
        let status = *(promise.add(crate::promise::STATUS as usize) as *const i32) as u32;
        let exception =
            *(promise.add(crate::promise::VALUE as usize) as *const u64) as usize as *mut u8;
        describe(&mut text, status, exception);
    }
    print_bytes(1, text.as_slice(), true);
    current().outcome = Some(0);
}

/// The reason `promise` was rejected with, as text (`TypeError: message`): what an export
/// reports as its WIT error.
///
/// # Safety
/// `promise` must be a rejected promise of the current task.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_rejection_text(promise: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let (status, value) = unsafe {
        crate::promise::perry_promise_rethrow(promise);
        (
            *(promise.add(crate::promise::STATUS as usize) as *const i32) as u32,
            *(promise.add(crate::promise::VALUE as usize) as *const u64),
        )
    };
    let mut text = Bytes::new();
    // SAFETY: a rejection's value is its exception object or null.
    unsafe {
        describe(&mut text, status, value as usize as *mut u8);
        crate::string::perry_string_adopt(text.ptr, text.len as u32)
    }
}

#[inline(never)]
unsafe fn describe(text: &mut Bytes, status: u32, exception: *mut u8) {
    if exception.is_null() {
        push_error_name(text, status);
        return;
    }
    // SAFETY: exception objects have the layout of crate::exception.
    let (kind, tag, value) = unsafe {
        (
            *(exception.add(KIND as usize) as *const u32),
            *(exception.add(TAG as usize) as *const u32),
            *(exception.add(VALUE as usize) as *const u64),
        )
    };
    let string = |value: u64| -> &'static [u8] {
        // SAFETY: tag says the slot holds a string.
        unsafe { crate::string::bytes(value as usize as *mut u8) }
    };
    match (kind, tag) {
        (TYPE_ERROR..=ERROR | REFERENCE_ERROR | SYNTAX_ERROR, _) => {
            push_error_name(text, kind);
            let message = string(value);
            if !message.is_empty() {
                text.push_str(": ");
                text.extend_from_slice(message);
            }
        }
        (_, TAG_NUMBER) => {
            // Integral values print exactly; others only approximately, without pulling
            // float formatting into every command.
            let number = f64::from_bits(value);
            if number.is_nan() {
                text.push_str("NaN");
            } else if number.is_infinite() {
                text.push_str(if number > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                });
            } else {
                // Exact for integral values; six decimals otherwise.
                let scaled = number.abs() * 1e6 + 0.5;
                if number < 0.0 {
                    text.push(b'-');
                }
                if scaled >= 9.0e18 {
                    text.push_str("(large number)");
                } else {
                    let scaled = scaled as u64;
                    push_digits(text, scaled / 1_000_000, 1);
                    let fraction = scaled % 1_000_000;
                    if fraction != 0 {
                        text.push(b'.');
                        let mut fraction = fraction;
                        let mut width = 6;
                        while fraction.is_multiple_of(10) {
                            fraction /= 10;
                            width -= 1;
                        }
                        push_digits(text, fraction, width);
                    }
                }
            }
        }
        (_, TAG_UNDEFINED) => text.push_str("undefined"),
        (_, TAG_NULL) => text.push_str("null"),
        (_, TAG_JSON) => text.push_str("[object Object]"),
        (_, TAG_BOOLEAN) => text.push_str(if value != 0 { "true" } else { "false" }),
        (_, TAG_STRING) => text.extend_from_slice(string(value)),
        _ => text.push_str("exception"),
    }
}

/// The constructor name of an `Error` status.
fn push_error_name(text: &mut Bytes, status: u32) {
    const PREFIXES: &str = "TypeRangeReferenceSyntax";
    // Packed byte ranges keep the lookup free of relocations in small linked modules.
    const STARTS: u64 = 0x1209000000040000;
    const SIZES: u64 = 0x609000000050400;
    let shift = if status < 8 { status * 8 } else { 0 };
    let start = ((STARTS >> shift) & 255) as usize;
    let size = ((SIZES >> shift) & 255) as usize;
    // SAFETY: every packed range selects an ASCII prefix from PREFIXES.
    let prefix = unsafe {
        core::str::from_utf8_unchecked(PREFIXES.as_bytes().get_unchecked(start..start + size))
    };
    text.push_str(prefix);
    text.push_str("Error");
}

fn push_digits(text: &mut Bytes, mut value: u64, width: usize) {
    let mut digits = [0u8; 20];
    let mut count = 0;
    while value > 0 || count < width {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    for &digit in digits[..count].iter().rev() {
        text.push(digit);
    }
}

/// The environment as `(name, value)` byte slices, lowered into the task region.
fn environment() -> &'static [[u32; 4]] {
    let mut list = [0u32; 2];
    // SAFETY: the host lowers the list through cabi_realloc into the current task.
    unsafe {
        host::get_environment(list.as_mut_ptr());
        if list[1] == 0 {
            return &[];
        }
        core::slice::from_raw_parts(list[0] as usize as *const [u32; 4], list[1] as usize)
    }
}

fn adopt(ptr: u32, len: u32) -> *mut u8 {
    // SAFETY: the host lowered valid UTF-8 into task memory.
    unsafe { crate::string::perry_string_adopt(ptr as usize as *mut u8, len) }
}

fn release_environment(entries: &[[u32; 4]]) {
    // SAFETY: all entries and the outer list came from canonical lowering.
    unsafe {
        for [key, _, value, _] in entries {
            crate::region::perry_cabi_release(*key as usize as *mut u8);
            crate::region::perry_cabi_release(*value as usize as *mut u8);
        }
        if !entries.is_empty() {
            crate::region::perry_cabi_release(entries.as_ptr().cast_mut().cast());
        }
    }
}

/// `process.env[name]`: the value, or null for `undefined`.
///
/// # Safety
/// `name` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_env_get(name: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let name = unsafe { crate::string::bytes(name) };
    let entries = environment();
    let mut found = core::ptr::null_mut();
    for [key, key_len, value, value_len] in entries {
        // SAFETY: entries point to live canonical storage; adoption does not collect.
        let key =
            unsafe { core::slice::from_raw_parts(*key as usize as *const u8, *key_len as usize) };
        if key == name {
            found = adopt(*value, *value_len);
        }
    }
    release_environment(entries);
    found
}

/// `Object.keys(process.env)`: an array of strings.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_env_keys() -> *mut u8 {
    let entries = environment();
    let array = crate::array::perry_array_new_typed(entries.len().max(1) as u32, 1);
    for (index, [key, key_len, ..]) in entries.iter().enumerate() {
        let string = adopt(*key, *key_len);
        // SAFETY: the array has capacity for every entry (layout of crate::array).
        unsafe {
            let data = *(array.add(crate::array::DATA as usize) as *const *mut u8);
            *(data.add(index * crate::array::SLOT as usize) as *mut u64) = string as usize as u64;
        }
    }
    // SAFETY: as above.
    unsafe { *(array.add(crate::array::LEN as usize) as *mut u32) = entries.len() as u32 };
    release_environment(entries);
    array
}

/// `performance.now()`: milliseconds on the monotonic clock.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_performance_now() -> f64 {
    // SAFETY: WASI import.
    unsafe { host::monotonic_now() as f64 / 1e6 }
}

/// `Date.now()`: integral milliseconds since the epoch.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_date_now() -> f64 {
    let mut instant = [0u64; 2];
    // SAFETY: WASI import writing `{ seconds: s64, nanoseconds: u32 }`.
    unsafe { host::system_now(instant.as_mut_ptr()) };
    let seconds = instant[0] as i64;
    let nanos = instant[1] as u32;
    (seconds as f64) * 1000.0 + f64::from(nanos / 1_000_000)
}
