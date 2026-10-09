//! Preopen-confined asynchronous filesystem operations, owned by one export task.
use crate::{
    exception, import_buffer, promise,
    region::{self, current, perry_alloc},
    string,
};
use alloc::string::String;
use core::ptr;

#[link(wasm_import_module = "wasi:filesystem/preopens@0.3.0")]
unsafe extern "C" {
    #[link_name = "get-directories"]
    fn directories(result: *mut Preopens);
}
#[link(wasm_import_module = "wasi:filesystem/types@0.3.0")]
unsafe extern "C" {
    #[link_name = "[resource-drop]descriptor"]
    fn drop_descriptor(descriptor: u32);
    #[link_name = "[async-lower][method]descriptor.stat-at"]
    fn stat(descriptor: u32, flags: u32, path: *const u8, len: u32, result: *mut StatResult)
    -> u32;
    #[link_name = "[async-lower][method]descriptor.create-directory-at"]
    fn mkdir(descriptor: u32, path: *const u8, len: u32, result: *mut UnitResult) -> u32;
    #[link_name = "[async-lower][method]descriptor.unlink-file-at"]
    fn unlink(descriptor: u32, path: *const u8, len: u32, result: *mut UnitResult) -> u32;
    #[link_name = "[async-lower][method]descriptor.remove-directory-at"]
    fn rmdir(descriptor: u32, path: *const u8, len: u32, result: *mut UnitResult) -> u32;
}
#[link(wasm_import_module = "$root")]
unsafe extern "C" {
    #[link_name = "[waitable-set-new]"]
    fn set_new() -> u32;
    #[link_name = "[waitable-join]"]
    fn join(handle: u32, set: u32);
    #[link_name = "[subtask-drop]"]
    fn subtask_drop(handle: u32);
    #[link_name = "[subtask-cancel]"]
    fn subtask_cancel(handle: u32) -> u32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Text {
    data: *const u8,
    len: u32,
}
#[repr(C)]
struct Preopen {
    descriptor: u32,
    path: Text,
}
#[repr(C)]
struct Preopens {
    entries: *const Preopen,
    len: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct OptionalText {
    tag: u8,
    value: Text,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Variant {
    tag: u8,
    other: OptionalText,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Instant {
    seconds: i64,
    nanos: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct OptionalInstant {
    tag: u8,
    value: Instant,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Stat {
    kind: Variant,
    links: u64,
    size: u64,
    access: OptionalInstant,
    modification: OptionalInstant,
    change: OptionalInstant,
}
#[repr(C)]
union Payload<T: Copy> {
    ok: T,
    error: Variant,
}
#[repr(C)]
struct Returned<T: Copy> {
    tag: u8,
    value: Payload<T>,
}
type StatResult = Returned<Stat>;
type UnitResult = Returned<()>;
#[repr(C)]
union ResultArea {
    stat: core::mem::ManuallyDrop<StatResult>,
    unit: core::mem::ManuallyDrop<UnitResult>,
}

#[derive(Clone, Copy)]
enum Kind {
    Stat,
    Mkdir,
    Unlink,
    Rmdir,
}
struct Operation {
    next: *mut Operation,
    handle: u32,
    directory: u32,
    promise: *mut u8,
    kind: Kind,
    result: ResultArea,
}
struct Stats {
    size: f64,
    mtime: f64,
    kind: u8,
}

fn normalize(path: &str) -> Result<String, u32> {
    if path.contains('\0') {
        return Err(12);
    }
    let absolute = path.starts_with('/');
    let mut result = String::with_capacity(path.len());
    if absolute {
        result.push('/');
    }
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if result.len() == usize::from(absolute) {
                    if !absolute {
                        return Err(1);
                    }
                } else {
                    result.truncate(result.rfind('/').unwrap_or(0).max(usize::from(absolute)));
                }
            }
            part => {
                if result.len() > usize::from(absolute) {
                    result.push('/');
                }
                result.push_str(part);
            }
        }
    }
    if result.is_empty() {
        result.push('.');
    }
    Ok(result)
}

fn relative<'a>(path: &'a str, mount: &str) -> Option<&'a str> {
    if mount == "/" {
        return Some(path.strip_prefix('/').unwrap_or(path));
    }
    if mount == "." && !path.starts_with('/') {
        return Some(path);
    }
    if path == mount {
        return Some("");
    }
    path.strip_prefix(mount)?.strip_prefix('/')
}

enum Path {
    Root(u32),
    Child(u32, Text),
}

unsafe fn resolve(path: *mut u8) -> Result<Path, u32> {
    // SAFETY: the input is live and canonical preopens are owned import buffers; unselected handles close here.
    unsafe {
        let path = normalize(string::text(path))?;
        let mut preopens = Preopens {
            entries: ptr::null(),
            len: 0,
        };
        directories(&mut preopens);
        let mut selected: Option<(u32, usize, &str)> = None;
        for index in 0..preopens.len as usize {
            let entry = &*preopens.entries.add(index);
            let mount = if entry.path.len == 0 {
                ""
            } else {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                    entry.path.data,
                    entry.path.len as usize,
                ))
            };
            if let Ok(mount) = normalize(mount)
                && let Some(relative) = relative(&path, &mount)
                && selected.is_none_or(|(_, score, _)| mount.len() > score)
            {
                if let Some((descriptor, _, _)) = selected {
                    drop_descriptor(descriptor);
                }
                selected = Some((entry.descriptor, mount.len(), relative));
            } else {
                drop_descriptor(entry.descriptor);
            }
            import_buffer::release(entry.path.data.cast_mut());
        }
        import_buffer::release(preopens.entries.cast_mut().cast());
        let (directory, _, path) = selected.ok_or(1u32)?;
        if path.is_empty() || path == "." {
            return Ok(Path::Root(directory));
        }
        let data = perry_alloc(path.len() as u32);
        ptr::copy_nonoverlapping(path.as_ptr(), data, path.len());
        Ok(Path::Child(
            directory,
            Text {
                data,
                len: path.len() as u32,
            },
        ))
    }
}

fn reject(promise: *mut u8, code: u32) {
    let exception = exception::perry_exception_new(exception::NUMBER, (code as f64).to_bits());
    // SAFETY: freshly allocated exception belongs to the current task.
    let status = unsafe { exception::perry_exception_raise(exception) };
    promise::perry_promise_reject(promise, status);
}

unsafe fn release_result(operation: *mut Operation) {
    // SAFETY: a returned canonical result owns only the optional string in its active variant.
    unsafe {
        let value = match (*operation).kind {
            Kind::Stat => {
                let result = &(*operation).result.stat;
                if result.tag == 0 {
                    let kind = result.value.ok.kind;
                    if kind.tag == 7 {
                        Some(kind.other)
                    } else {
                        None
                    }
                } else {
                    let error = result.value.error;
                    if error.tag == 36 {
                        Some(error.other)
                    } else {
                        None
                    }
                }
            }
            _ => {
                let error = &(*operation).result.unit;
                if error.tag != 0 && error.value.error.tag == 36 {
                    Some(error.value.error.other)
                } else {
                    None
                }
            }
        };
        if let Some(text) = value
            && text.tag != 0
        {
            import_buffer::release(text.value.data.cast_mut());
        }
    }
}

unsafe fn complete(operation: *mut Operation) {
    // SAFETY: the host has returned; output memory and the borrowed descriptor belong to this operation.
    unsafe {
        release_result(operation);
        let operation = &mut *operation;
        let value = match operation.kind {
            Kind::Stat => {
                let result = &operation.result.stat;
                if result.tag != 0 {
                    Err(u32::from(result.value.error.tag) + 1)
                } else {
                    let stat = result.value.ok;
                    let stats = perry_alloc(size_of::<Stats>() as u32).cast::<Stats>();
                    stats.write(Stats {
                        size: stat.size as f64,
                        mtime: if stat.modification.tag == 0 {
                            0.0
                        } else {
                            stat.modification.value.seconds as f64 * 1000.0
                                + stat.modification.value.nanos as f64 / 1e6
                        },
                        kind: stat.kind.tag,
                    });
                    Ok(stats as usize as u64)
                }
            }
            _ => {
                let result = &operation.result.unit;
                if result.tag != 0 {
                    Err(u32::from(result.value.error.tag) + 1)
                } else {
                    Ok(crate::UNDEFINED_NUMBER)
                }
            }
        };
        drop_descriptor(operation.directory);
        match value {
            Ok(value) => promise::perry_promise_resolve(operation.promise, value),
            Err(error) => reject(operation.promise, error),
        }
    }
}

unsafe fn start(path: *mut u8, kind: Kind, valid: bool) -> *mut u8 {
    let promise = promise::perry_promise_new();
    if !valid {
        reject(promise, 12);
        return promise;
    }
    // SAFETY: source and all allocated operation data belong to the current task.
    unsafe {
        let (directory, path) = match resolve(path) {
            Ok(Path::Child(directory, path)) => (directory, path),
            Ok(Path::Root(directory)) if matches!(kind, Kind::Stat) => (
                directory,
                Text {
                    data: b".".as_ptr(),
                    len: 1,
                },
            ),
            Ok(Path::Root(directory)) => {
                drop_descriptor(directory);
                reject(promise, if matches!(kind, Kind::Mkdir) { 7 } else { 31 });
                return promise;
            }
            Err(error) => {
                reject(promise, error);
                return promise;
            }
        };
        let operation = perry_alloc(size_of::<Operation>() as u32).cast::<Operation>();
        operation.write(Operation {
            next: ptr::null_mut(),
            handle: 0,
            directory,
            promise,
            kind,
            result: core::mem::zeroed(),
        });
        let result = &raw mut (*operation).result;
        let status = match kind {
            Kind::Stat => stat(directory, 1, path.data, path.len, result.cast()),
            Kind::Mkdir => mkdir(directory, path.data, path.len, result.cast()),
            Kind::Unlink => unlink(directory, path.data, path.len, result.cast()),
            Kind::Rmdir => rmdir(directory, path.data, path.len, result.cast()),
        };
        if status & 15 == 2 {
            complete(operation);
        } else {
            (*operation).handle = status >> 4;
            let task = current();
            (*operation).next = task.filesystem.cast();
            task.filesystem = operation.cast();
            if task.set == 0 {
                task.set = set_new();
            }
            join((*operation).handle, task.set);
        }
    }
    promise
}

macro_rules! operation {
    ($name:ident, $kind:ident) => {
        /// # Safety
        /// `path` is a live UTF-8 string in the current task.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(path: *mut u8, valid: u32) -> *mut u8 {
            // SAFETY: caller guarantees the live string.
            unsafe { start(path, Kind::$kind, valid != 0) }
        }
    };
}
operation!(perry_fs_stat, Stat);
operation!(perry_fs_mkdir, Mkdir);
operation!(perry_fs_unlink, Unlink);
operation!(perry_fs_rmdir, Rmdir);

#[unsafe(no_mangle)]
pub extern "C" fn perry_fs_event(event: u32, handle: u32, state: u32) {
    if event != 1 || state != 2 {
        return;
    }
    let mut link = &raw mut current().filesystem;
    // SAFETY: each list node owns exactly one active subtask and its result area.
    unsafe {
        while !(*link).is_null() {
            let operation = (*link).cast::<Operation>();
            if (*operation).handle == handle {
                *link = (*operation).next.cast();
                subtask_drop(handle);
                complete(operation);
                return;
            }
            link = (&raw mut (*operation).next).cast();
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn perry_fs_cancel() {
    let task = current() as *mut region::Task;
    // SAFETY: cancellation acknowledges all borrowed native storage before task reclamation.
    unsafe {
        let mut operation =
            core::mem::replace(&mut (*task).filesystem, ptr::null_mut()).cast::<Operation>();
        while !operation.is_null() {
            let handle = (*operation).handle;
            join(handle, 0);
            let status = subtask_cancel(handle);
            region::perry_task_resume(task);
            if status == 2 {
                release_result(operation);
            }
            subtask_drop(handle);
            drop_descriptor((*operation).directory);
            operation = (*operation).next;
        }
    }
}

/// # Safety
/// `stats` is a live filesystem Stats value; field is size (0), mtimeMs (1), or kind (2).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_fs_stats_get(stats: *mut u8, field: u32) -> f64 {
    // SAFETY: typed MIR enforces the receiver and field.
    let stats = unsafe { &*stats.cast::<Stats>() };
    match field {
        0 => stats.size,
        1 => stats.mtime,
        2 => f64::from(stats.kind),
        _ => unreachable!(),
    }
}
