//! `wasi:filesystem@0.3.0` imports, named with the legacy mangling `wit-component` resolves.
//!
//! Async functions are async-lowered: they take their flat parameters (or a pointer to them
//! when there are more than four) and a result area, and return a subtask status. Streams and
//! futures are read and written with the canonical built-ins of the function that created
//! them; payload indices count the function's streams and futures in signature order.

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "wasi:filesystem/preopens@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-directories"]
        pub fn get_directories(result: *mut u32);
    }

    #[link(wasm_import_module = "wasi:filesystem/types@0.3.0")]
    unsafe extern "C" {
        #[link_name = "[resource-drop]descriptor"]
        pub fn descriptor_drop(descriptor: u32);
        #[link_name = "[async-lower][method]descriptor.open-at"]
        pub fn open_at(params: *const u8, result: *mut u8) -> u32;
        #[link_name = "[async-lower][method]descriptor.stat-at"]
        pub fn stat_at(fd: u32, flags: u32, path: *const u8, len: u32, result: *mut u8) -> u32;
        #[link_name = "[async-lower][method]descriptor.create-directory-at"]
        pub fn create_directory_at(fd: u32, path: *const u8, len: u32, result: *mut u8) -> u32;
        #[link_name = "[async-lower][method]descriptor.remove-directory-at"]
        pub fn remove_directory_at(fd: u32, path: *const u8, len: u32, result: *mut u8) -> u32;
        #[link_name = "[async-lower][method]descriptor.unlink-file-at"]
        pub fn unlink_file_at(fd: u32, path: *const u8, len: u32, result: *mut u8) -> u32;

        #[link_name = "[method]descriptor.read-via-stream"]
        pub fn read_via_stream(fd: u32, offset: u64, result: *mut u32);
        #[link_name = "[async-lower][stream-read-0][method]descriptor.read-via-stream"]
        pub fn bytes_read(stream: u32, buffer: *mut u8, len: u32) -> u32;
        #[link_name = "[stream-drop-readable-0][method]descriptor.read-via-stream"]
        pub fn bytes_read_drop(stream: u32);
        #[link_name = "[async-lower][future-read-1][method]descriptor.read-via-stream"]
        pub fn bytes_read_result(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-drop-readable-1][method]descriptor.read-via-stream"]
        pub fn bytes_read_result_drop(future: u32);

        #[link_name = "[stream-new-0][method]descriptor.write-via-stream"]
        pub fn bytes_write_new() -> u64;
        #[link_name = "[method]descriptor.write-via-stream"]
        pub fn write_via_stream(fd: u32, data: u32, offset: u64) -> u32;
        #[link_name = "[async-lower][stream-write-0][method]descriptor.write-via-stream"]
        pub fn bytes_write(stream: u32, data: *const u8, len: u32) -> u32;
        #[link_name = "[stream-drop-writable-0][method]descriptor.write-via-stream"]
        pub fn bytes_write_drop(stream: u32);
        #[link_name = "[async-lower][future-read-1][method]descriptor.write-via-stream"]
        pub fn bytes_write_result(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-drop-readable-1][method]descriptor.write-via-stream"]
        pub fn bytes_write_result_drop(future: u32);

        #[link_name = "[method]descriptor.read-directory"]
        pub fn read_directory(fd: u32, result: *mut u32);
        #[link_name = "[async-lower][stream-read-0][method]descriptor.read-directory"]
        pub fn entries_read(stream: u32, buffer: *mut u8, count: u32) -> u32;
        #[link_name = "[stream-drop-readable-0][method]descriptor.read-directory"]
        pub fn entries_read_drop(stream: u32);
        #[link_name = "[async-lower][future-read-1][method]descriptor.read-directory"]
        pub fn entries_read_result(future: u32, result: *mut u8) -> u32;
        #[link_name = "[future-drop-readable-1][method]descriptor.read-directory"]
        pub fn entries_read_result_drop(future: u32);
    }
}

#[cfg(target_arch = "wasm32")]
pub use imports::*;

/// On the host nothing is preopened, so operations reject before calling the others.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
mod host {
    pub unsafe fn get_directories(result: *mut u32) {
        unsafe {
            *result = 0;
            *result.add(1) = 0;
        }
    }
    macro_rules! absent {
        ($($name:ident($($arg:ty),*) $(-> $result:ty)?;)*) => {
            $(pub unsafe fn $name($(_: $arg),*) $(-> $result)? {
                unreachable!("no host filesystem")
            })*
        };
    }
    absent! {
        descriptor_drop(u32);
        open_at(*const u8, *mut u8) -> u32;
        stat_at(u32, u32, *const u8, u32, *mut u8) -> u32;
        create_directory_at(u32, *const u8, u32, *mut u8) -> u32;
        remove_directory_at(u32, *const u8, u32, *mut u8) -> u32;
        unlink_file_at(u32, *const u8, u32, *mut u8) -> u32;
        read_via_stream(u32, u64, *mut u32);
        bytes_read(u32, *mut u8, u32) -> u32;
        bytes_read_drop(u32);
        bytes_read_result(u32, *mut u8) -> u32;
        bytes_read_result_drop(u32);
        bytes_write_new() -> u64;
        write_via_stream(u32, u32, u64) -> u32;
        bytes_write(u32, *const u8, u32) -> u32;
        bytes_write_drop(u32);
        bytes_write_result(u32, *mut u8) -> u32;
        bytes_write_result_drop(u32);
        read_directory(u32, *mut u32);
        entries_read(u32, *mut u8, u32) -> u32;
        entries_read_drop(u32);
        entries_read_result(u32, *mut u8) -> u32;
        entries_read_result_drop(u32);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use host::*;

/// `descriptor-flags`.
pub const READ: u8 = 1;
pub const WRITE: u8 = 2;
/// `open-flags`.
pub const CREATE: u8 = 1;
pub const DIRECTORY: u8 = 2;
pub const TRUNCATE: u8 = 8;
/// `path-flags`.
pub const SYMLINK_FOLLOW: u8 = 1;

/// `descriptor-type` cases.
pub const TYPE_DIRECTORY: u8 = 2;
pub const TYPE_REGULAR_FILE: u8 = 5;

/// Parameters of the async-lowered `open-at` (more than four flat values).
#[repr(C)]
pub struct OpenAt {
    pub fd: u32,
    pub path_flags: u8,
    pub path: *const u8,
    pub len: u32,
    pub open_flags: u8,
    pub flags: u8,
}

#[cfg(target_arch = "wasm32")]
const _: () = assert!(core::mem::size_of::<OpenAt>() == 20);

/// A `directory-entry` in a stream buffer: `type` at 0, `name` at 16.
pub const ENTRY_SIZE: usize = 24;
pub const ENTRY_NAME: usize = 16;
/// `result<descriptor-stat, error-code>`: the payload at 8; `size` at 24 within the stat.
pub const STAT_PAYLOAD: usize = 8;
pub const STAT_SIZE: usize = 24;
