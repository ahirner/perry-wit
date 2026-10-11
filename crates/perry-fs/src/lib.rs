//! `node:fs/promises` over `wasi:filesystem@0.3.0`: `readFile`, `writeFile`, `stat`,
//! `readdir`, `mkdir`, `unlink` and `rmdir`.
//!
//! A helper library like `perry-std`, linked only into programs that import `node:fs`. Each
//! call starts an operation that returns its promise at once and advances whenever the host
//! completes a step: async-lowered calls finish as subtasks, file contents and directory
//! entries arrive on streams, and each stream's result on a future. The waitables join the
//! task's set; compiled callbacks pass their events to [fs_event] first.
//!
//! Paths resolve as in WASI libc with the working directory `/`: against the preopen with the
//! longest matching prefix. Every operation takes its own preopen handles and drops them when
//! it settles, so no host resource outlives the call. Failures reject with Node's message
//! (`ENOENT: no such file or directory, open 'missing.txt'`).
#![cfg_attr(target_arch = "wasm32", no_std)]

mod canonical;
mod wasi;

use core::{cell::UnsafeCell, ptr};
use perry_abi::{
    BLOCKED, Buffer, DROPPED, EVENT_FUTURE_READ, EVENT_STREAM_READ, EVENT_STREAM_WRITE,
    EVENT_SUBTASK, SUBTASK_RETURNED, UNDEFINED_BITS, adopt, alloc, bytes, bytes_view, from_bytes,
    join, promise_new, reject, resolve, subtask_drop, unjoin, view_bytes,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    ReadText,
    ReadBytes,
    Write,
    Stat,
    Readdir,
    Mkdir,
    Unlink,
    Rmdir,
}

impl Kind {
    /// The system call Node names in messages.
    fn syscall(self) -> &'static str {
        match self {
            Kind::ReadText | Kind::ReadBytes | Kind::Write => "open",
            Kind::Stat => "stat",
            Kind::Readdir => "scandir",
            Kind::Mkdir => "mkdir",
            Kind::Unlink => "unlink",
            Kind::Rmdir => "rmdir",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Start,
    Opened,
    Reading,
    ReadResult,
    Writing,
    WriteResult,
    Listing,
    ListResult,
    Finished,
    Done,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    Nothing,
    Subtask,
    Stream,
    Future,
}

struct Op {
    task: *mut u8,
    pin: u32,
    kind: Kind,
    stage: Stage,
    promise: *mut u8,
    /// The path as given, for messages.
    path: *mut u8,
    /// The path relative to `dir`.
    target: Buffer,
    /// Bytes to write, copied at the call.
    data: Buffer,
    written: usize,
    /// The preopen and the opened descriptor (0 when none is owned).
    dir: u32,
    file: u32,
    wait: Wait,
    waiting: u32,
    stream: u32,
    future: u32,
    /// Result area of async calls and futures (8-aligned, large enough for a stat result).
    area: *mut u8,
    /// File contents, or directory entry names (string addresses).
    contents: Buffer,
    next: *mut Op,
}

struct Ops(UnsafeCell<*mut Op>);
// SAFETY: one instance runs one thread.
unsafe impl Sync for Ops {}
static OPS: Ops = Ops(UnsafeCell::new(ptr::null_mut()));

fn ops() -> &'static mut *mut Op {
    // SAFETY: single-threaded; no reference outlives one entry point.
    unsafe { &mut *OPS.0.get() }
}

const AREA: u32 = 112;
const CHUNK: usize = 16 * 1024;
const ENTRIES: usize = 16;

fn start(kind: Kind, path: *mut u8, data: &[u8]) -> *mut u8 {
    let op = perry_abi::object!(Op; promise, path, target, data, area, contents);
    let promise = promise_new();
    let mut copy = Buffer::new();
    copy.extend_from_slice(data);
    // SAFETY: a fresh block for the operation, linked into the list until it settles.
    unsafe {
        op.write(Op {
            task: perry_abi::task(),
            pin: perry_abi::pin(op.cast()),
            kind,
            stage: Stage::Start,
            promise,
            path,
            target: Buffer::new(),
            data: copy,
            written: 0,
            dir: 0,
            file: 0,
            wait: Wait::Nothing,
            waiting: 0,
            stream: 0,
            future: 0,
            area: alloc(AREA),
            contents: Buffer::new(),
            next: *ops(),
        });
        *ops() = op;
        advance(&mut *op, None);
    }
    promise
}

/// `readFile(path, 'utf8')`: the contents as a string (invalid UTF-8 replaced).
#[unsafe(no_mangle)]
pub extern "C" fn fs_read_text(path: *mut u8) -> *mut u8 {
    start(Kind::ReadText, path, &[])
}

/// `readFile(path)`: the contents as a `Uint8Array`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_read_bytes(path: *mut u8) -> *mut u8 {
    start(Kind::ReadBytes, path, &[])
}

/// `writeFile(path, text)`: create or truncate, then write the UTF-8 bytes.
///
/// # Safety
/// `text` must be a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_write_text(path: *mut u8, text: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    start(Kind::Write, path, unsafe { bytes(text) })
}

/// `writeFile(path, view)`: the view's bytes as they are at the call.
///
/// # Safety
/// `view` must be a `Uint8Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_write_bytes(path: *mut u8, view: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    start(Kind::Write, path, unsafe { view_bytes(view) })
}

/// `stat(path)`: a stats object ([fs_stats_is_file], [fs_stats_size]).
#[unsafe(no_mangle)]
pub extern "C" fn fs_stat(path: *mut u8) -> *mut u8 {
    start(Kind::Stat, path, &[])
}

/// `readdir(path)`: entry names in host order.
#[unsafe(no_mangle)]
pub extern "C" fn fs_readdir(path: *mut u8) -> *mut u8 {
    start(Kind::Readdir, path, &[])
}

/// `mkdir(path)`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_mkdir(path: *mut u8) -> *mut u8 {
    start(Kind::Mkdir, path, &[])
}

/// `unlink(path)`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_unlink(path: *mut u8) -> *mut u8 {
    start(Kind::Unlink, path, &[])
}

/// `rmdir(path)`.
#[unsafe(no_mangle)]
pub extern "C" fn fs_rmdir(path: *mut u8) -> *mut u8 {
    start(Kind::Rmdir, path, &[])
}

/// Stats: `type: u32` at 0 (a `descriptor-type` case), `size: f64` at 8.
#[repr(C)]
struct Stats {
    kind: u32,
    size: f64,
}

/// `stats.isFile()`.
///
/// # Safety
/// `stats` must come from [fs_stat].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_stats_is_file(stats: *mut u8) -> u32 {
    // SAFETY: guaranteed by the caller.
    u32::from(unsafe { (*(stats as *const Stats)).kind } == u32::from(wasi::TYPE_REGULAR_FILE))
}

/// `stats.isDirectory()`.
///
/// # Safety
/// `stats` must come from [fs_stat].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_stats_is_directory(stats: *mut u8) -> u32 {
    // SAFETY: guaranteed by the caller.
    u32::from(unsafe { (*(stats as *const Stats)).kind } == u32::from(wasi::TYPE_DIRECTORY))
}

/// `stats.size`.
///
/// # Safety
/// `stats` must come from [fs_stat].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn fs_stats_size(stats: *mut u8) -> f64 {
    // SAFETY: guaranteed by the caller.
    unsafe { (*(stats as *const Stats)).size }
}

/// A callback event of the current task: advance the operation waiting on `handle`.
/// Returns 1 if the event was one of an operation.
#[unsafe(no_mangle)]
pub extern "C" fn fs_event(event: u32, handle: u32, code: u32) -> u32 {
    let wait = match event {
        EVENT_SUBTASK => Wait::Subtask,
        EVENT_STREAM_READ | EVENT_STREAM_WRITE => Wait::Stream,
        EVENT_FUTURE_READ => Wait::Future,
        _ => return 0,
    };
    let mut op = *ops();
    // SAFETY: listed operations are live until they settle.
    unsafe {
        while !op.is_null() {
            if (*op).task == perry_abi::task() && (*op).wait == wait && (*op).waiting == handle {
                if wait == Wait::Subtask {
                    if code != SUBTASK_RETURNED {
                        return 1;
                    }
                    unjoin(handle);
                    subtask_drop(handle);
                }
                (*op).wait = Wait::Nothing;
                advance(&mut *op, Some(code));
                return 1;
            }
            op = (*op).next;
        }
    }
    0
}

/// Cancel this task's outstanding operations before releasing any borrowed buffer.
/// Returns the number of detached calls, for the task-boundary diagnostic.
#[unsafe(no_mangle)]
pub extern "C" fn fs_close() -> u32 {
    let task = perry_abi::task();
    let mut cursor = *ops();
    let mut cancelled = 0;
    // SAFETY: each listed operation remains pinned through acknowledgement and unlinking.
    unsafe {
        while !cursor.is_null() {
            let op = &mut *cursor;
            if op.task == task {
                cancelled += 1;
                if op.wait != Wait::Nothing {
                    unjoin(op.waiting);
                }
                match op.wait {
                    Wait::Subtask => {
                        let status = perry_abi::subtask_cancel(op.waiting);
                        if status == SUBTASK_RETURNED {
                            canonical::release_result(op);
                        }
                        if status == SUBTASK_RETURNED
                            && op.stage == Stage::Opened
                            && byte(op.area, 0) == 0
                        {
                            op.file = word(op.area, 4);
                        }
                        subtask_drop(op.waiting);
                    }
                    Wait::Stream => {
                        let copied = perry_abi::acknowledged(|| match op.kind {
                            Kind::Write => wasi::bytes_write_cancel(op.stream),
                            Kind::Readdir => wasi::entries_read_cancel(op.stream),
                            _ => wasi::bytes_read_cancel(op.stream),
                        });
                        if op.kind == Kind::Readdir {
                            op.contents
                                .advance((copied >> 4) as usize * wasi::ENTRY_SIZE);
                        }
                    }
                    Wait::Future => {
                        let result = perry_abi::acknowledged(|| match op.kind {
                            Kind::Write => wasi::bytes_write_result_cancel(op.future),
                            Kind::Readdir => wasi::entries_read_result_cancel(op.future),
                            _ => wasi::bytes_read_result_cancel(op.future),
                        });
                        if result == 0 {
                            canonical::release_result(op);
                        }
                    }
                    Wait::Nothing => {}
                }
                if op.stream != 0 {
                    match op.kind {
                        Kind::Write => wasi::bytes_write_drop(op.stream),
                        Kind::Readdir => wasi::entries_read_drop(op.stream),
                        _ => wasi::bytes_read_drop(op.stream),
                    }
                    op.stream = 0;
                }
                if op.future != 0 {
                    match op.kind {
                        Kind::Write => wasi::bytes_write_result_drop(op.future),
                        Kind::Readdir => wasi::entries_read_result_drop(op.future),
                        _ => wasi::bytes_read_result_drop(op.future),
                    }
                    op.future = 0;
                }
                op.wait = Wait::Nothing;
                finish(op);
                cursor = *ops();
            } else {
                cursor = op.next;
            }
        }
    }
    cancelled
}

/// Wait for an async-lowered call that returned `status`; false if it already returned.
fn called(op: &mut Op, status: u32) -> bool {
    if status & 0xF == SUBTASK_RETURNED {
        return false;
    }
    let subtask = status >> 4;
    join(subtask);
    op.wait = Wait::Subtask;
    op.waiting = subtask;
    true
}

/// Wait on a stream or future that blocked.
fn blocked(op: &mut Op, wait: Wait, handle: u32) {
    join(handle);
    op.wait = wait;
    op.waiting = handle;
}

fn word(area: *mut u8, offset: usize) -> u32 {
    // SAFETY: within the result area.
    unsafe { *(area.add(offset) as *const u32) }
}

fn byte(area: *mut u8, offset: usize) -> u8 {
    // SAFETY: within the result area.
    unsafe { *area.add(offset) }
}

/// Run `op` until it waits or settles; `code` is the result of the step it waited for.
fn advance(op: &mut Op, mut code: Option<u32>) {
    loop {
        match op.stage {
            Stage::Start => {
                if !open_preopen(op) {
                    return fail(op, NO_ENTRY);
                }
                let (target, len) = (op.target.as_slice().as_ptr(), op.target.len() as u32);
                let status = match op.kind {
                    Kind::ReadText | Kind::ReadBytes | Kind::Write | Kind::Readdir => {
                        if op.kind == Kind::Readdir && op.target.as_slice() == b"." {
                            op.stage = Stage::Listing;
                            start_listing(op, op.dir);
                            continue;
                        }
                        let (open_flags, flags) = match op.kind {
                            Kind::Write => (wasi::CREATE | wasi::TRUNCATE, wasi::WRITE),
                            Kind::Readdir => (wasi::DIRECTORY, wasi::READ),
                            _ => (0, wasi::READ),
                        };
                        let params =
                            alloc(core::mem::size_of::<wasi::OpenAt>() as u32) as *mut wasi::OpenAt;
                        // SAFETY: a fresh block; the host reads it during the call.
                        unsafe {
                            params.write(wasi::OpenAt {
                                fd: op.dir,
                                path_flags: wasi::SYMLINK_FOLLOW,
                                path: target,
                                len,
                                open_flags,
                                flags,
                            });
                            wasi::open_at(params as *const u8, op.area)
                        }
                    }
                    // SAFETY: async-lowered calls with a live path and result area.
                    Kind::Stat => unsafe {
                        wasi::stat_at(
                            op.dir,
                            u32::from(wasi::SYMLINK_FOLLOW),
                            target,
                            len,
                            op.area,
                        )
                    },
                    Kind::Mkdir => unsafe {
                        wasi::create_directory_at(op.dir, target, len, op.area)
                    },
                    Kind::Unlink => unsafe { wasi::unlink_file_at(op.dir, target, len, op.area) },
                    Kind::Rmdir => unsafe {
                        wasi::remove_directory_at(op.dir, target, len, op.area)
                    },
                };
                op.stage = match op.kind {
                    Kind::ReadText | Kind::ReadBytes | Kind::Write | Kind::Readdir => Stage::Opened,
                    _ => Stage::Finished,
                };
                if called(op, status) {
                    return;
                }
            }
            Stage::Opened => {
                canonical::release_result(op);
                if byte(op.area, 0) != 0 {
                    return fail(op, byte(op.area, 4));
                }
                op.file = word(op.area, 4);
                match op.kind {
                    Kind::Write => {
                        // SAFETY: canonical built-ins of `write-via-stream`.
                        let ends = unsafe { wasi::bytes_write_new() };
                        op.stream = (ends >> 32) as u32;
                        op.future = unsafe { wasi::write_via_stream(op.file, ends as u32, 0) };
                        op.stage = Stage::Writing;
                    }
                    Kind::Readdir => {
                        op.stage = Stage::Listing;
                        start_listing(op, op.file);
                    }
                    _ => {
                        let mut ends = [0u32; 2];
                        // SAFETY: the result area holds the stream and future handles.
                        unsafe { wasi::read_via_stream(op.file, 0, ends.as_mut_ptr()) };
                        (op.stream, op.future) = (ends[0], ends[1]);
                        op.stage = Stage::Reading;
                    }
                }
            }
            Stage::Reading => {
                let result = match code.take() {
                    Some(result) => result,
                    None => {
                        op.contents.reserve(CHUNK);
                        let (spare, room) = op.contents.spare();
                        // SAFETY: the reserved bytes stay valid until the copy completes.
                        let result = unsafe { wasi::bytes_read(op.stream, spare, room as u32) };
                        if result == BLOCKED {
                            return blocked(op, Wait::Stream, op.stream);
                        }
                        result
                    }
                };
                op.contents.advance((result >> 4) as usize);
                if result & 0xF == DROPPED {
                    unjoin(op.stream);
                    // SAFETY: the readable end this operation owns.
                    unsafe { wasi::bytes_read_drop(op.stream) };
                    op.stream = 0;
                    op.stage = Stage::ReadResult;
                }
            }
            Stage::ReadResult => {
                if code.take().is_none() {
                    // SAFETY: the future of `read-via-stream`, read into the result area.
                    let result = unsafe { wasi::bytes_read_result(op.future, op.area) };
                    if result == BLOCKED {
                        return blocked(op, Wait::Future, op.future);
                    }
                }
                unjoin(op.future);
                // SAFETY: the readable end this operation owns.
                unsafe { wasi::bytes_read_result_drop(op.future) };
                op.future = 0;
                canonical::release_result(op);
                if byte(op.area, 0) != 0 {
                    return fail(op, byte(op.area, 4));
                }
                let contents = op.contents.as_slice();
                let value = if op.kind == Kind::ReadText {
                    text(contents)
                } else {
                    bytes_view(contents.len() as u32, contents.as_ptr() as *mut u8)
                };
                return succeed(op, value as usize as u64);
            }
            Stage::Writing => {
                if let Some(result) = code.take() {
                    op.written += (result >> 4) as usize;
                    if result & 0xF == DROPPED {
                        op.stage = Stage::WriteResult;
                    }
                }
                if op.stage == Stage::Writing && op.written < op.data.len() {
                    let rest = &op.data.as_slice()[op.written..];
                    // SAFETY: the data buffer outlives the operation.
                    let result =
                        unsafe { wasi::bytes_write(op.stream, rest.as_ptr(), rest.len() as u32) };
                    if result == BLOCKED {
                        return blocked(op, Wait::Stream, op.stream);
                    }
                    code = Some(result);
                    continue;
                }
                unjoin(op.stream);
                // SAFETY: the writable end this operation owns.
                unsafe { wasi::bytes_write_drop(op.stream) };
                op.stream = 0;
                op.stage = Stage::WriteResult;
            }
            Stage::WriteResult => {
                if code.take().is_none() {
                    // SAFETY: the future of `write-via-stream`, read into the result area.
                    let result = unsafe { wasi::bytes_write_result(op.future, op.area) };
                    if result == BLOCKED {
                        return blocked(op, Wait::Future, op.future);
                    }
                }
                unjoin(op.future);
                // SAFETY: the readable end this operation owns.
                unsafe { wasi::bytes_write_result_drop(op.future) };
                op.future = 0;
                canonical::release_result(op);
                if byte(op.area, 0) != 0 {
                    return fail(op, byte(op.area, 4));
                }
                if op.written < op.data.len() {
                    return fail(op, wasi::ERROR_OTHER);
                }
                return succeed(op, UNDEFINED_BITS);
            }
            Stage::Listing => {
                let result = match code.take() {
                    Some(result) => result,
                    None => {
                        op.contents.reserve(ENTRIES * wasi::ENTRY_SIZE);
                        let (spare, room) = op.contents.spare();
                        let count = (room / wasi::ENTRY_SIZE) as u32;
                        // SAFETY: the reserved entries stay valid until the copy completes.
                        let result = unsafe { wasi::entries_read(op.stream, spare, count) };
                        if result == BLOCKED {
                            return blocked(op, Wait::Stream, op.stream);
                        }
                        result
                    }
                };
                op.contents
                    .advance((result >> 4) as usize * wasi::ENTRY_SIZE);
                if result & 0xF == DROPPED {
                    unjoin(op.stream);
                    // SAFETY: the readable end this operation owns.
                    unsafe { wasi::entries_read_drop(op.stream) };
                    op.stream = 0;
                    op.stage = Stage::ListResult;
                }
            }
            Stage::ListResult => {
                if code.take().is_none() {
                    // SAFETY: the future of `read-directory`, read into the result area.
                    let result = unsafe { wasi::entries_read_result(op.future, op.area) };
                    if result == BLOCKED {
                        return blocked(op, Wait::Future, op.future);
                    }
                }
                unjoin(op.future);
                // SAFETY: the readable end this operation owns.
                unsafe { wasi::entries_read_result_drop(op.future) };
                op.future = 0;
                canonical::release_result(op);
                if byte(op.area, 0) != 0 {
                    return fail(op, byte(op.area, 4));
                }
                let entries = op.contents.as_slice();
                let mut names = Buffer::new();
                for entry in entries.as_chunks::<{ wasi::ENTRY_SIZE }>().0 {
                    let at = entry.as_ptr() as *mut u8;
                    let (data, len) = (word(at, wasi::ENTRY_NAME), word(at, wasi::ENTRY_NAME + 4));
                    // SAFETY: the host lowered the name into this task's region as UTF-8.
                    let name = unsafe { adopt(data as usize as *mut u8, len) };
                    if matches!(unsafe { bytes(name) }, b"." | b"..") {
                        continue;
                    }
                    names.extend_from_slice(&(name as usize as u64).to_le_bytes());
                }
                let slots = names.as_slice();
                // SAFETY: 8-byte slots written above, in a block from the task region.
                let slots = unsafe {
                    core::slice::from_raw_parts(slots.as_ptr() as *const u64, slots.len() / 8)
                };
                let array = perry_abi::array(slots);
                return succeed(op, array as usize as u64);
            }
            Stage::Finished => {
                canonical::release_result(op);
                let payload = if op.kind == Kind::Stat {
                    wasi::STAT_PAYLOAD
                } else {
                    4
                };
                if byte(op.area, 0) != 0 {
                    return fail(op, byte(op.area, payload));
                }
                if op.kind != Kind::Stat {
                    return succeed(op, UNDEFINED_BITS);
                }
                let stat = unsafe { op.area.add(payload) };
                // SAFETY: a `descriptor-stat` in the result area; `size` is 8-aligned.
                let size = unsafe { *(stat.add(wasi::STAT_SIZE) as *const u64) };
                let stats = alloc(core::mem::size_of::<Stats>() as u32) as *mut Stats;
                // SAFETY: a fresh block.
                unsafe {
                    stats.write(Stats {
                        kind: u32::from(byte(stat, 0)),
                        size: size as f64,
                    })
                };
                return succeed(op, stats as usize as u64);
            }
            Stage::Done => return,
        }
    }
}

fn start_listing(op: &mut Op, fd: u32) {
    let mut ends = [0u32; 2];
    // SAFETY: the result area holds the stream and future handles.
    unsafe { wasi::read_directory(fd, ends.as_mut_ptr()) };
    (op.stream, op.future) = (ends[0], ends[1]);
}

/// Contents as a string, replacing invalid UTF-8 as `TextDecoder` does.
fn text(contents: &[u8]) -> *mut u8 {
    if core::str::from_utf8(contents).is_ok() {
        // SAFETY: valid UTF-8 in the task region.
        return unsafe { adopt(contents.as_ptr() as *mut u8, contents.len() as u32) };
    }
    let mut out = Buffer::new();
    for chunk in contents.utf8_chunks() {
        out.extend_from_slice(chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            out.extend_from_slice("\u{FFFD}".as_bytes());
        }
    }
    let out = out.as_slice();
    // SAFETY: valid UTF-8 in the task region.
    unsafe { adopt(out.as_ptr() as *mut u8, out.len() as u32) }
}

/// Take preopen handles, keep the one whose name is the longest prefix of the path, drop the
/// others, and store the rest of the path in `op.target`.
fn open_preopen(op: &mut Op) -> bool {
    // SAFETY: the path argument is a string.
    let path = unsafe { bytes(op.path) };
    let mut list = [0u32; 2];
    // SAFETY: the host lowers the list into the task region.
    unsafe { wasi::get_directories(list.as_mut_ptr()) };
    let (entries, count) = (list[0] as usize as *const u32, list[1] as usize);
    let mut best: Option<(usize, u32, usize)> = None;
    for index in 0..count {
        // SAFETY: `count` entries of (descriptor, name pointer, name length).
        let (fd, name) = unsafe {
            let entry = entries.add(index * 3);
            let len = *entry.add(2) as usize;
            let name: &[u8] = if len == 0 {
                &[]
            } else {
                core::slice::from_raw_parts(*entry.add(1) as usize as *const u8, len)
            };
            (*entry, name)
        };
        match prefix(name, path) {
            Some(depth) if best.is_none_or(|(d, _, _)| depth > d) => {
                if let Some((_, old, _)) = best {
                    drop_descriptor(old);
                }
                best = Some((depth, fd, index));
            }
            _ => drop_descriptor(fd),
        }
        if !name.is_empty() {
            unsafe { perry_abi::release_canonical(name.as_ptr().cast_mut()) };
        }
    }
    unsafe { perry_abi::release_canonical(entries.cast_mut().cast()) };
    let Some((depth, fd, _)) = best else {
        return false;
    };
    op.dir = fd;
    let mut skipped = 0;
    for segment in segments(path) {
        if skipped < depth {
            skipped += 1;
            continue;
        }
        if !op.target.is_empty() {
            op.target.push(b'/');
        }
        op.target.extend_from_slice(segment);
    }
    if op.target.is_empty() {
        op.target.push(b'.');
    }
    true
}

/// Segments of `path` from the root (the working directory is `/`) without `.` and empty
/// ones. `..` stays for the host to resolve, as POSIX does: `a/../b` needs `a` to exist.
fn segments(path: &[u8]) -> impl Iterator<Item = &[u8]> {
    path.split(|&b| b == b'/')
        .filter(|segment| !matches!(*segment, b"" | b"."))
}

/// The number of segments of preopen `name` if it is a prefix of `path`.
fn prefix(name: &[u8], path: &[u8]) -> Option<usize> {
    let mut path = segments(path);
    let mut depth = 0;
    for segment in segments(name) {
        if path.next() != Some(segment) {
            return None;
        }
        depth += 1;
    }
    Some(depth)
}

fn drop_descriptor(fd: u32) {
    // SAFETY: a descriptor this operation owns.
    unsafe { wasi::descriptor_drop(fd) };
}

/// Release the operation's descriptors and unlink it.
fn finish(op: &mut Op) {
    if op.kind == Kind::Readdir {
        canonical::release_entries(op.contents.as_slice());
    }
    if op.file != 0 {
        drop_descriptor(op.file);
        op.file = 0;
    }
    if op.dir != 0 {
        drop_descriptor(op.dir);
        op.dir = 0;
    }
    perry_abi::unpin(core::mem::take(&mut op.pin));
    op.stage = Stage::Done;
    let this = op as *mut Op;
    let mut link: *mut *mut Op = ops();
    // SAFETY: the list holds live operations.
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

fn succeed(op: &mut Op, value: u64) {
    finish(op);
    resolve(op.promise, value);
}

const NO_ENTRY: u8 = 19;

/// Node's code and description for a `wasi:filesystem` `error-code` case.
fn describe(code: u8) -> (&'static str, &'static str) {
    match code {
        0 => ("EACCES", "permission denied"),
        1 => ("EALREADY", "operation already in progress"),
        2 => ("EBADF", "bad file descriptor"),
        3 => ("EBUSY", "resource busy or locked"),
        6 => ("EEXIST", "file already exists"),
        7 => ("EFBIG", "file too large"),
        11 => ("EINVAL", "invalid argument"),
        13 => ("EISDIR", "illegal operation on a directory"),
        14 => ("ELOOP", "too many symbolic links encountered"),
        17 => ("ENAMETOOLONG", "name too long"),
        19 => ("ENOENT", "no such file or directory"),
        22 => ("ENOSPC", "no space left on device"),
        23 => ("ENOTDIR", "not a directory"),
        24 => ("ENOTEMPTY", "directory not empty"),
        26 => ("ENOTSUP", "operation not supported on socket"),
        30 => ("EPERM", "operation not permitted"),
        32 => ("EROFS", "read-only file system"),
        35 => ("EXDEV", "cross-device link not permitted"),
        _ => ("EIO", "i/o error"),
    }
}

/// Reject with `CODE: description, syscall 'path'`.
fn fail(op: &mut Op, code: u8) {
    finish(op);
    let (name, description) = describe(code);
    let mut message = Buffer::new();
    for part in [name, ": ", description, ", ", op.kind.syscall(), " '"] {
        message.extend_from_slice(part.as_bytes());
    }
    // SAFETY: the path argument is a string.
    message.extend_from_slice(unsafe { bytes(op.path) });
    message.push(b'\'');
    reject(op.promise, from_bytes(message.as_slice()));
}
