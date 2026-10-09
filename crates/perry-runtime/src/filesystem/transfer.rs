//! File and directory transfers own descriptors, buffers and completion futures until acknowledged.
use super::*;
use crate::{array, bytes};

#[repr(C)]
struct OpenArgs {
    directory: u32,
    path_flags: u32,
    path: Text,
    open_flags: u32,
    flags: u32,
}
#[repr(C)]
struct Reader {
    stream: u32,
    future: u32,
}
#[repr(C)]
struct Entry {
    kind: Variant,
    name: Text,
}

#[link(wasm_import_module = "wasi:filesystem/types@0.3.0")]
unsafe extern "C" {
    #[link_name = "[async-lower][method]descriptor.open-at"]
    fn open(args: *const OpenArgs, result: *mut Returned<u32>) -> u32;
    #[link_name = "[method]descriptor.read-via-stream"]
    fn read(file: u32, offset: u64, result: *mut Reader);
    #[link_name = "[async-lower][stream-read-0][method]descriptor.read-via-stream"]
    fn read_bytes(reader: u32, data: *mut u8, len: u32) -> u32;
    #[link_name = "[stream-drop-readable-0][method]descriptor.read-via-stream"]
    fn drop_reader(reader: u32);
    #[link_name = "[stream-cancel-read-0][method]descriptor.read-via-stream"]
    fn cancel_reader(reader: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.read-via-stream"]
    fn read_completion(future: u32, result: *mut UnitResult) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.read-via-stream"]
    fn drop_read_completion(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.read-via-stream"]
    fn cancel_read_completion(future: u32) -> u32;

    #[link_name = "[stream-new-0][method]descriptor.write-via-stream"]
    fn new_writer() -> u64;
    #[link_name = "[method]descriptor.write-via-stream"]
    fn write(file: u32, reader: u32, offset: u64) -> u32;
    #[link_name = "[async-lower][stream-write-0][method]descriptor.write-via-stream"]
    fn write_bytes(writer: u32, data: *const u8, len: u32) -> u32;
    #[link_name = "[stream-drop-writable-0][method]descriptor.write-via-stream"]
    fn drop_writer(writer: u32);
    #[link_name = "[stream-cancel-write-0][method]descriptor.write-via-stream"]
    fn cancel_writer(writer: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.write-via-stream"]
    fn write_completion(future: u32, result: *mut UnitResult) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.write-via-stream"]
    fn drop_write_completion(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.write-via-stream"]
    fn cancel_write_completion(future: u32) -> u32;

    #[link_name = "[method]descriptor.read-directory"]
    fn directory(file: u32, result: *mut Reader);
    #[link_name = "[async-lower][stream-read-0][method]descriptor.read-directory"]
    fn read_entries(reader: u32, data: *mut Entry, len: u32) -> u32;
    #[link_name = "[stream-drop-readable-0][method]descriptor.read-directory"]
    fn drop_directory(reader: u32);
    #[link_name = "[stream-cancel-read-0][method]descriptor.read-directory"]
    fn cancel_directory(reader: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.read-directory"]
    fn directory_completion(future: u32, result: *mut UnitResult) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.read-directory"]
    fn drop_directory_completion(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.read-directory"]
    fn cancel_directory_completion(future: u32) -> u32;
}

#[derive(Clone, Copy)]
enum Mode {
    Bytes,
    Text,
    Write,
    Directory,
}
#[derive(Clone, Copy)]
enum Phase {
    Opening,
    Reading {
        reader: u32,
        future: u32,
    },
    Writing {
        writer: u32,
        future: u32,
        offset: u32,
    },
    Completion {
        future: u32,
        short: bool,
    },
}
pub(super) struct Transfer {
    mode: Mode,
    phase: Phase,
    args: OpenArgs,
    data: *mut u8,
    len: u32,
    capacity: u32,
    entries: *mut u8,
    entry: Entry,
    completion: UnitResult,
}

unsafe fn release_variant(value: Variant, other: u8) {
    // SAFETY: only the active other variant owns imported string storage.
    unsafe {
        if value.tag == other && value.other.tag != 0 {
            import_buffer::release(value.other.value.data.cast_mut());
        }
    }
}
unsafe fn completion_error(value: &UnitResult) -> u32 {
    // SAFETY: the future returned and initialized the active canonical variant.
    unsafe {
        if value.tag == 0 {
            0
        } else {
            let error = value.value.error;
            release_variant(error, 36);
            u32::from(error.tag) + 1
        }
    }
}
unsafe fn release_entry(entry: &Entry) {
    // SAFETY: a returned directory entry owns its canonical strings.
    unsafe {
        release_variant(entry.kind, 7);
        import_buffer::release(entry.name.data.cast_mut());
    }
}

unsafe fn finish(operation: *mut Operation, transfer: *mut Transfer, mut error: u32) {
    // SAFETY: all native transfers have stopped; only owned buffers and descriptor remain.
    unsafe {
        let state = &mut *transfer;
        let mut value = crate::UNDEFINED_NUMBER;
        if error == 0 {
            value = match state.mode {
                Mode::Bytes => bytes::perry_bytes_adopt(state.data, state.len) as u64,
                Mode::Text => {
                    let input = if state.len == 0 {
                        &[]
                    } else {
                        core::slice::from_raw_parts(state.data, state.len as usize)
                    };
                    if core::str::from_utf8(input).is_err() {
                        error = 9;
                        0
                    } else {
                        string::perry_string_adopt(state.data, state.len) as u64
                    }
                }
                Mode::Directory => state.entries as u64,
                Mode::Write => crate::UNDEFINED_NUMBER,
            };
        }
        if error == 0 && matches!(state.mode, Mode::Bytes | Mode::Text) {
            import_buffer::claim(state.data);
        } else {
            import_buffer::release(state.data);
        }
        state.data = ptr::null_mut();
        drop_descriptor((*operation).directory);
        detach(operation);
        if error == 0 {
            promise::perry_promise_resolve((*operation).promise, value);
        } else {
            reject((*operation).promise, error);
        }
    }
}

pub(super) unsafe fn opened(operation: *mut Operation, transfer: *mut Transfer) {
    // SAFETY: open returned and the parent descriptor is no longer borrowed by it.
    unsafe {
        let result = &(*operation).result.open;
        if result.tag != 0 {
            let error = result.value.error;
            release_variant(error, 36);
            finish(operation, transfer, u32::from(error.tag) + 1);
            return;
        }
        drop_descriptor((*operation).directory);
        (*operation).directory = result.value.ok;
        (*operation).handle = 0;
        let state = &mut *transfer;
        state.phase = match state.mode {
            Mode::Write => {
                let pair = new_writer();
                let future = write((*operation).directory, pair as u32, 0);
                Phase::Writing {
                    writer: (pair >> 32) as u32,
                    future,
                    offset: 0,
                }
            }
            Mode::Bytes | Mode::Text | Mode::Directory => {
                let mut result = Reader {
                    stream: 0,
                    future: 0,
                };
                if matches!(state.mode, Mode::Directory) {
                    directory((*operation).directory, &mut result);
                } else {
                    read((*operation).directory, 0, &mut result);
                }
                Phase::Reading {
                    reader: result.stream,
                    future: result.future,
                }
            }
        };
        drive(operation, transfer, None);
    }
}

unsafe fn drive(operation: *mut Operation, transfer: *mut Transfer, mut event: Option<u32>) {
    // SAFETY: each phase retains its native endpoints and stable memory until completion.
    unsafe {
        let state = &mut *transfer;
        loop {
            match state.phase {
                Phase::Opening => unreachable!("open completion was not dispatched"),
                Phase::Reading { reader, future } => {
                    let listing = matches!(state.mode, Mode::Directory);
                    let result = if let Some(result) = event.take() {
                        result
                    } else if listing {
                        read_entries(reader, &mut state.entry, 1)
                    } else {
                        if state.capacity - state.len < 8192 {
                            let capacity = state
                                .capacity
                                .checked_mul(2)
                                .unwrap()
                                .max(state.len.checked_add(8192).unwrap());
                            state.data = import_buffer::perry_import_realloc(
                                state.data,
                                state.capacity,
                                8,
                                capacity,
                            );
                            state.capacity = capacity;
                        }
                        read_bytes(
                            reader,
                            state.data.add(state.len as usize),
                            state.capacity - state.len,
                        )
                    };
                    if result == u32::MAX {
                        wait(reader);
                        return;
                    }
                    let count = result >> 4;
                    if listing {
                        assert!(count <= 1, "directory read exceeded capacity");
                        if count != 0 {
                            let name = state.entry.name;
                            let bytes = if name.len == 0 {
                                &[]
                            } else {
                                core::slice::from_raw_parts(name.data, name.len as usize)
                            };
                            if matches!(bytes, b"." | b"..") {
                                release_entry(&state.entry);
                            } else {
                                import_buffer::claim(name.data.cast_mut());
                                release_variant(state.entry.kind, 7);
                                let name =
                                    string::perry_string_adopt(name.data.cast_mut(), name.len);
                                let len = array::slots(state.entries).len();
                                array::set_slot(state.entries, len, name as u64);
                            }
                        }
                    } else {
                        assert!(
                            count <= state.capacity - state.len,
                            "file read exceeded capacity"
                        );
                        state.len += count;
                    }
                    if result & 15 != 0 {
                        if listing {
                            drop_directory(reader);
                        } else {
                            drop_reader(reader);
                        }
                        state.phase = Phase::Completion {
                            future,
                            short: false,
                        };
                    }
                }
                Phase::Writing {
                    writer,
                    future,
                    offset,
                } => {
                    let result = event.take().unwrap_or_else(|| {
                        write_bytes(writer, state.data.add(offset as usize), state.len - offset)
                    });
                    if result == u32::MAX {
                        wait(writer);
                        return;
                    }
                    let count = result >> 4;
                    assert!(count <= state.len - offset, "file write exceeded capacity");
                    let offset = offset + count;
                    if result & 15 != 0 || offset == state.len {
                        drop_writer(writer);
                        state.phase = Phase::Completion {
                            future,
                            short: offset != state.len,
                        };
                    } else {
                        state.phase = Phase::Writing {
                            writer,
                            future,
                            offset,
                        };
                    }
                }
                Phase::Completion { future, short } => {
                    let result = event.take().unwrap_or_else(|| match state.mode {
                        Mode::Write => write_completion(future, &mut state.completion),
                        Mode::Directory => directory_completion(future, &mut state.completion),
                        _ => read_completion(future, &mut state.completion),
                    });
                    if result == u32::MAX {
                        wait(future);
                        return;
                    }
                    let error = if result == 0 {
                        completion_error(&state.completion)
                    } else {
                        32
                    };
                    match state.mode {
                        Mode::Write => drop_write_completion(future),
                        Mode::Directory => drop_directory_completion(future),
                        _ => drop_read_completion(future),
                    }
                    finish(
                        operation,
                        transfer,
                        if error == 0 && short { 32 } else { error },
                    );
                    return;
                }
            }
        }
    }
}

unsafe fn event(
    operation: *mut Operation,
    transfer: *mut Transfer,
    event: u32,
    handle: u32,
    result: u32,
) -> bool {
    // SAFETY: the active phase identifies the sole pending waitable for this operation.
    unsafe {
        match (*transfer).phase {
            Phase::Opening if event == 1 && result == 2 && handle == (*operation).handle => {
                subtask_drop(handle);
                opened(operation, transfer);
            }
            Phase::Reading { reader, .. } if event == 2 && handle == reader => {
                drive(operation, transfer, Some(result))
            }
            Phase::Writing { writer, .. } if event == 3 && handle == writer => {
                drive(operation, transfer, Some(result))
            }
            Phase::Completion { future, .. } if event == 4 && handle == future => {
                drive(operation, transfer, Some(result))
            }
            _ => return false,
        }
    }
    true
}

unsafe fn cancel(operation: *mut Operation, transfer: *mut Transfer) {
    let task = current() as *mut region::Task;
    // SAFETY: cancellation acknowledges borrowed memory before it or any descriptor is released.
    unsafe {
        let state = &mut *transfer;
        match state.phase {
            Phase::Opening => {
                join((*operation).handle, 0);
                let status = subtask_cancel((*operation).handle);
                region::perry_task_resume(task);
                if status == 2 {
                    let result = &(*operation).result.open;
                    if result.tag == 0 {
                        drop_descriptor(result.value.ok);
                    } else {
                        release_variant(result.value.error, 36);
                    }
                }
                subtask_drop((*operation).handle);
            }
            Phase::Reading { reader, future } => {
                join(reader, 0);
                if matches!(state.mode, Mode::Directory) {
                    let result = cancel_directory(reader);
                    region::perry_task_resume(task);
                    if result >> 4 != 0 {
                        release_entry(&state.entry);
                    }
                    drop_directory(reader);
                    drop_directory_completion(future);
                } else {
                    cancel_reader(reader);
                    region::perry_task_resume(task);
                    drop_reader(reader);
                    drop_read_completion(future);
                }
            }
            Phase::Writing { writer, future, .. } => {
                join(writer, 0);
                cancel_writer(writer);
                region::perry_task_resume(task);
                drop_writer(writer);
                drop_write_completion(future);
            }
            Phase::Completion { future, .. } => {
                join(future, 0);
                let result = match state.mode {
                    Mode::Write => cancel_write_completion(future),
                    Mode::Directory => cancel_directory_completion(future),
                    _ => cancel_read_completion(future),
                };
                region::perry_task_resume(task);
                if result == 0 {
                    completion_error(&state.completion);
                }
                match state.mode {
                    Mode::Write => drop_write_completion(future),
                    Mode::Directory => drop_directory_completion(future),
                    _ => drop_read_completion(future),
                }
            }
        }
        drop_descriptor((*operation).directory);
        import_buffer::release(state.data);
        state.data = ptr::null_mut();
    }
}

unsafe fn start(path: *mut u8, data: *const u8, len: u32, mode: Mode, valid: bool) -> *mut u8 {
    let promise = promise::perry_promise_new();
    if !valid {
        reject(promise, 12);
        return promise;
    }
    // SAFETY: the caller supplies live arguments; operation storage outlives all native calls.
    unsafe {
        let (directory, path) = match resolve(path) {
            Ok(Path::Child(directory, path)) => (directory, path),
            Ok(Path::Root(directory)) if matches!(mode, Mode::Directory) => (
                directory,
                Text {
                    data: b".".as_ptr(),
                    len: 1,
                },
            ),
            Ok(Path::Root(directory)) => {
                drop_descriptor(directory);
                reject(promise, 14);
                return promise;
            }
            Err(error) => {
                reject(promise, error);
                return promise;
            }
        };
        let transfer = perry_alloc(size_of::<Transfer>() as u32).cast::<Transfer>();
        let mut state = Transfer {
            mode,
            phase: Phase::Opening,
            args: OpenArgs {
                directory,
                path_flags: 1,
                path,
                open_flags: match mode {
                    Mode::Write => 9,
                    Mode::Directory => 2,
                    _ => 0,
                },
                flags: if matches!(mode, Mode::Write) { 2 } else { 1 },
            },
            data: ptr::null_mut(),
            len: 0,
            capacity: 0,
            entries: ptr::null_mut(),
            entry: core::mem::zeroed(),
            completion: core::mem::zeroed(),
        };
        if matches!(mode, Mode::Write) {
            state.data = import_buffer::perry_import_realloc(ptr::null_mut(), 0, 8, len.max(1));
            ptr::copy_nonoverlapping(data, state.data, len as usize);
            state.len = len;
            state.capacity = len.max(1);
        }
        if matches!(mode, Mode::Directory) {
            state.entries = array::perry_array_new(4);
        }
        transfer.write(state);
        let operation = perry_alloc(size_of::<Operation>() as u32).cast::<Operation>();
        operation.write(Operation {
            next: current().filesystem.cast(),
            handle: 0,
            directory,
            promise,
            kind: Kind::Transfer(transfer),
            result: core::mem::zeroed(),
        });
        current().filesystem = operation.cast();
        let status = open(&(*transfer).args, (&raw mut (*operation).result).cast());
        if status & 15 == 2 {
            opened(operation, transfer);
        } else {
            (*operation).handle = status >> 4;
            wait((*operation).handle);
        }
    }
    promise
}

/// # Safety
/// `path` is a live task-owned string; text selects strict UTF-8 rather than bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_fs_read(path: *mut u8, text: u32, valid: u32) -> *mut u8 {
    // SAFETY: guaranteed by the typed caller.
    unsafe {
        start(
            path,
            ptr::NonNull::<u8>::dangling().as_ptr(),
            0,
            if text != 0 { Mode::Text } else { Mode::Bytes },
            valid != 0,
        )
    }
}
/// # Safety
/// `path` and `data` are live strings or (when text is zero) a byte view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_fs_write(
    path: *mut u8,
    data: *mut u8,
    text: u32,
    valid: u32,
) -> *mut u8 {
    // SAFETY: typed MIR determines the data representation and keeps arguments live.
    unsafe {
        let (data, len) = if text != 0 {
            let data = string::bytes(data);
            (data.as_ptr(), data.len() as u32)
        } else {
            (
                bytes::perry_bytes_data(data).cast_const(),
                bytes::perry_bytes_length(data) as u32,
            )
        };
        start(path, data, len, Mode::Write, valid != 0)
    }
}
/// # Safety
/// `path` is a live task-owned string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_fs_readdir(path: *mut u8, valid: u32) -> *mut u8 {
    // SAFETY: guaranteed by the typed caller.
    unsafe {
        start(
            path,
            ptr::NonNull::<u8>::dangling().as_ptr(),
            0,
            Mode::Directory,
            valid != 0,
        )
    }
}

/// # Safety
/// Non-null encoding and flag values are live UTF-8 strings; mode is emitted by typed lowering.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_fs_options(
    encoding: *mut u8,
    flag: *mut u8,
    mode: f64,
    allowed: u32,
) -> u32 {
    if allowed == 0 {
        return 0;
    }
    // SAFETY: optional strings are null or valid live strings.
    unsafe {
        let flag = flag.is_null()
            || string::text(flag) == if mode == 2.0 || mode == 3.0 { "w" } else { "r" };
        let encoding = encoding.is_null() || {
            let label = string::text(encoding);
            let utf8 = label.eq_ignore_ascii_case("utf8") || label.eq_ignore_ascii_case("utf-8");
            let binary = label.eq_ignore_ascii_case("binary");
            if mode == 0.0 {
                binary
            } else if mode == 3.0 {
                binary || utf8
            } else {
                utf8
            }
        };
        u32::from(flag && encoding)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_fs_transfer_event(event_kind: u32, handle: u32, result: u32) {
    let mut operation = current().filesystem.cast::<Operation>();
    // SAFETY: all pending nodes are task-owned; matching one may finish and detach it.
    unsafe {
        while !operation.is_null() {
            if let Kind::Transfer(transfer) = (*operation).kind
                && event(operation, transfer, event_kind, handle, result)
            {
                return;
            }
            operation = (*operation).next;
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_fs_transfers_cancel() {
    let mut operation = current().filesystem.cast::<Operation>();
    // SAFETY: cancellation detaches only transfers, leaving metadata calls to their owner.
    unsafe {
        while !operation.is_null() {
            let next = (*operation).next;
            if let Kind::Transfer(transfer) = (*operation).kind {
                detach(operation);
                cancel(operation, transfer);
            }
            operation = next;
        }
    }
}
