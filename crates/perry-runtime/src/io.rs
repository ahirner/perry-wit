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
    exception::{EXIT, TAG_BOOLEAN, TAG_NUMBER, TAG_STRING},
    host::{self, BLOCKED, DROPPED, EVENT_FUTURE_READ, EVENT_STREAM_WRITE},
    region::current,
};
use core::cell::UnsafeCell;

/// A growable byte buffer in the task region: released with the task, so a component that
/// prints never needs the buffer allocator's `free` path.
pub(crate) struct Bytes {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl Bytes {
    pub(crate) const fn new() -> Self {
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
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Open,
    /// Writable end dropped; waiting for the completion future.
    Closing,
    Closed,
}

impl Output {
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
pub unsafe extern "C" fn perry_print(fd: u32, string: *mut u8, newline: u32) {
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { crate::string::bytes(string) };
    print_bytes(fd as usize, bytes, newline != 0);
}

fn print_bytes(fd: usize, bytes: &[u8], newline: bool) {
    let output = &mut current().outputs[fd.min(1)];
    if output.state != State::Open {
        return;
    }
    output.queued.extend_from_slice(bytes);
    if newline {
        output.queued.push(b'\n');
    }
}

fn finish_copy(output: &mut Output, code: u32) {
    output.written += (code >> 4) as usize;
    if code & 0xF == DROPPED {
        // The reader is gone: discard the rest, like writes to a closed pipe.
        output.written = output.writing.len();
        output.queued.clear();
    }
}

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
            output.writing = core::mem::replace(&mut output.queued, Bytes::new());
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
pub extern "C" fn perry_io_pump() {
    pump(0);
    pump(1);
}

/// Handle a stream or future event of the current task's output. Returns 1 if it was one.
#[unsafe(no_mangle)]
pub extern "C" fn perry_io_event(event: u32, handle: u32, code: u32) -> u32 {
    for fd in 0..2 {
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
    }
    0
}

/// Flush and close the current task's outputs. Returns 1 once everything reached the host;
/// otherwise the task waits on its set for the events that finish it.
#[unsafe(no_mangle)]
pub extern "C" fn perry_io_close() -> u32 {
    let mut done = 1;
    for fd in 0..2 {
        pump(fd);
        let output = &mut current().outputs[fd];
        match output.state {
            State::Open if output.writable == 0 => output.state = State::Closed,
            State::Open if output.idle() => {
                // SAFETY: canonical built-ins on handles this output owns.
                unsafe { host::waitable_join(output.writable, 0) };
                host::output::drop_writer(fd, output.writable);
                output.state = State::Closing;
                let code = host::output::read(fd, output.future, output.result.as_mut_ptr());
                if code == BLOCKED {
                    let future = output.future;
                    perry_task_join(future);
                    done = 0;
                } else {
                    host::output::drop_future(fd, output.future);
                    output.state = State::Closed;
                }
            }
            State::Closed => {}
            _ => done = 0,
        }
    }
    done
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
pub extern "C" fn perry_task_subtask_event(handle: u32, state: u32) {
    if state == host::SUBTASK_RETURNED {
        crate::region::perry_subtask_done(handle);
        // SAFETY: canonical built-in on a returned subtask of this task.
        unsafe { host::subtask_drop(handle) };
    }
}

/// Drop the current task's waitable set, if it has one.
#[unsafe(no_mangle)]
pub extern "C" fn perry_task_close_set() {
    let task = current();
    if task.set != 0 {
        // SAFETY: canonical built-in; nothing is joined any more.
        unsafe { host::waitable_set_drop(task.set) };
        task.set = 0;
    }
}

struct Process {
    exit_code: i32,
}

struct Global(UnsafeCell<Process>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Global {}
static PROCESS: Global = Global(UnsafeCell::new(Process { exit_code: 0 }));

#[allow(clippy::mut_from_ref)]
fn process() -> &'static mut Process {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *PROCESS.0.get() }
}

fn exit_code(code: f64) -> i32 {
    if code.is_finite() { code as i32 } else { 0 }
}

/// `process.exitCode = code`.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exit_code_set(code: f64) {
    process().exit_code = exit_code(code);
}

/// `process.exit(code)`: record the code and return the exit status that unwinds to the
/// export.
#[unsafe(no_mangle)]
pub extern "C" fn perry_exit(code: f64) -> i32 {
    process().exit_code = exit_code(code);
    EXIT as i32
}

/// The outcome of a command whose main promise settled: 0 for `ok`, 1 for `err`. An uncaught
/// exception is reported on stderr. Idempotent within the task.
///
/// # Safety
/// `promise` must be a settled promise of the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_command_outcome(promise: *mut u8) -> u32 {
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

unsafe fn describe(text: &mut Bytes, status: u32, exception: *mut u8) {
    if exception.is_null() {
        text.push_str(match status {
            1 => "TypeError",
            2 => "RangeError",
            _ => "Error",
        });
        return;
    }
    // SAFETY: exception objects have the layout of crate::exception.
    let (kind, tag, value) = unsafe {
        (
            *(exception as *const u32),
            *(exception.add(4) as *const u32),
            *(exception.add(8) as *const u64),
        )
    };
    let string = |value: u64| -> &'static [u8] {
        // SAFETY: tag says the slot holds a string.
        unsafe { crate::string::bytes(value as usize as *mut u8) }
    };
    match (kind, tag) {
        (1..=3, _) => {
            text.push_str(["TypeError", "RangeError", "Error"][kind as usize - 1]);
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
                let scaled = libm::round(libm::fabs(number) * 1e6);
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
        (_, TAG_BOOLEAN) => text.push_str(if value != 0 { "true" } else { "false" }),
        (_, TAG_STRING) => text.extend_from_slice(string(value)),
        _ => text.push_str("exception"),
    }
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

/// `process.env[name]`: the value, or null for `undefined`.
///
/// # Safety
/// `name` must be a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_env_get(name: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let name = unsafe { crate::string::bytes(name) };
    for [key, key_len, value, value_len] in environment() {
        // SAFETY: entries point into task memory.
        let key =
            unsafe { core::slice::from_raw_parts(*key as usize as *const u8, *key_len as usize) };
        if key == name {
            return adopt(*value, *value_len);
        }
    }
    core::ptr::null_mut()
}

/// `Object.keys(process.env)`: an array of strings.
#[unsafe(no_mangle)]
pub extern "C" fn perry_env_keys() -> *mut u8 {
    let entries = environment();
    let array = crate::array::perry_array_new(entries.len().max(1) as u32);
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
    array
}

/// `performance.now()`: milliseconds on the monotonic clock.
#[unsafe(no_mangle)]
pub extern "C" fn perry_performance_now() -> f64 {
    // SAFETY: WASI import.
    unsafe { host::monotonic_now() as f64 / 1e6 }
}

/// `Date.now()`: integral milliseconds since the epoch.
#[unsafe(no_mangle)]
pub extern "C" fn perry_date_now() -> f64 {
    let mut instant = [0u64; 2];
    // SAFETY: WASI import writing `{ seconds: s64, nanoseconds: u32 }`.
    unsafe { host::system_now(instant.as_mut_ptr()) };
    let seconds = instant[0] as i64;
    let nanos = instant[1] as u32;
    (seconds as f64) * 1000.0 + f64::from(nanos / 1_000_000)
}
