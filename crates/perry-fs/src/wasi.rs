//! `wasi:filesystem@0.3.0` imports, named with the legacy mangling `wit-component` resolves.
//!
//! Async functions are async-lowered: they take their flat parameters (or a pointer to them
//! when there are more than four) and a result area, and return a subtask status. Streams and
//! futures are read and written with the canonical built-ins of the function that created
//! them, and payload indices count the function's streams and futures in signature order.

perry_abi::wasm_imports! { "wasi:filesystem/preopens@0.3.0";
    #[link_name = "get-directories"]
    pub(crate) fn get_directories(result: *mut u32) => {
        unsafe {
            *result = 0;
            *result.add(1) = 0;
        }
    };
}

perry_abi::wasm_imports! { "wasi:filesystem/types@0.3.0";
    #[link_name = "[resource-drop]descriptor"]
    pub(crate) fn descriptor_drop(descriptor: u32);
    #[link_name = "[async-lower][method]descriptor.open-at"]
    pub(crate) fn open_at(params: *const u8, result: *mut u8) -> u32;
    #[link_name = "[async-lower][method]descriptor.stat-at"]
    pub(crate) fn stat_at(
        fd: u32,
        flags: u32,
        path: *const u8,
        len: u32,
        result: *mut u8,
    ) -> u32;
    #[link_name = "[async-lower][method]descriptor.create-directory-at"]
    pub(crate) fn create_directory_at(
        fd: u32,
        path: *const u8,
        len: u32,
        result: *mut u8,
    ) -> u32;
    #[link_name = "[async-lower][method]descriptor.remove-directory-at"]
    pub(crate) fn remove_directory_at(
        fd: u32,
        path: *const u8,
        len: u32,
        result: *mut u8,
    ) -> u32;
    #[link_name = "[async-lower][method]descriptor.unlink-file-at"]
    pub(crate) fn unlink_file_at(fd: u32, path: *const u8, len: u32, result: *mut u8) -> u32;

    #[link_name = "[method]descriptor.read-via-stream"]
    pub(crate) fn read_via_stream(fd: u32, offset: u64, result: *mut u32);
    #[link_name = "[async-lower][stream-read-0][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read(stream: u32, buffer: *mut u8, len: u32) -> u32;
    #[link_name = "[stream-drop-readable-0][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read_drop(stream: u32);
    #[link_name = "[stream-cancel-read-0][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read_cancel(handle: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read_result(future: u32, result: *mut u8) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read_result_drop(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.read-via-stream"]
    pub(crate) fn bytes_read_result_cancel(handle: u32) -> u32;

    #[link_name = "[stream-new-0][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_new() -> u64;
    #[link_name = "[method]descriptor.write-via-stream"]
    pub(crate) fn write_via_stream(fd: u32, data: u32, offset: u64) -> u32;
    #[link_name = "[async-lower][stream-write-0][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write(stream: u32, data: *const u8, len: u32) -> u32;
    #[link_name = "[stream-drop-writable-0][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_drop(stream: u32);
    #[link_name = "[stream-cancel-write-0][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_cancel(handle: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_result(future: u32, result: *mut u8) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_result_drop(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.write-via-stream"]
    pub(crate) fn bytes_write_result_cancel(handle: u32) -> u32;

    #[link_name = "[method]descriptor.read-directory"]
    pub(crate) fn read_directory(fd: u32, result: *mut u32);
    #[link_name = "[async-lower][stream-read-0][method]descriptor.read-directory"]
    pub(crate) fn entries_read(stream: u32, buffer: *mut u8, count: u32) -> u32;
    #[link_name = "[stream-drop-readable-0][method]descriptor.read-directory"]
    pub(crate) fn entries_read_drop(stream: u32);
    #[link_name = "[stream-cancel-read-0][method]descriptor.read-directory"]
    pub(crate) fn entries_read_cancel(handle: u32) -> u32;
    #[link_name = "[async-lower][future-read-1][method]descriptor.read-directory"]
    pub(crate) fn entries_read_result(future: u32, result: *mut u8) -> u32;
    #[link_name = "[future-drop-readable-1][method]descriptor.read-directory"]
    pub(crate) fn entries_read_result_drop(future: u32);
    #[link_name = "[future-cancel-read-1][method]descriptor.read-directory"]
    pub(crate) fn entries_read_result_cancel(handle: u32) -> u32;
}

/// `descriptor-flags`.
pub(crate) const READ: u8 = 1;
pub(crate) const WRITE: u8 = 2;
/// `open-flags`.
pub(crate) const CREATE: u8 = 1;
pub(crate) const DIRECTORY: u8 = 2;
pub(crate) const TRUNCATE: u8 = 8;
/// `path-flags`.
pub(crate) const SYMLINK_FOLLOW: u8 = 1;

/// `descriptor-type` cases.
pub(crate) const TYPE_DIRECTORY: u8 = 2;
pub(crate) const TYPE_REGULAR_FILE: u8 = 5;
pub(crate) const TYPE_OTHER: u8 = 7;
pub(crate) const ERROR_OTHER: u8 = 36;

/// Parameters of the async-lowered `open-at` (more than four flat values).
#[repr(C)]
pub(crate) struct OpenAt {
    pub(crate) fd: u32,
    pub(crate) path_flags: u8,
    pub(crate) path: *const u8,
    pub(crate) len: u32,
    pub(crate) open_flags: u8,
    pub(crate) flags: u8,
}

#[cfg(target_arch = "wasm32")]
const _: () = assert!(core::mem::size_of::<OpenAt>() == 20);

/// A `directory-entry` in a stream buffer: `type` at 0, `name` at 16.
pub(crate) const ENTRY_SIZE: usize = 24;
pub(crate) const ENTRY_NAME: usize = 16;
/// Layout of `result<descriptor-stat, error-code>` with the payload at offset 8 and `size` at offset 24 within the stat.
pub(crate) const STAT_PAYLOAD: usize = 8;
pub(crate) const STAT_SIZE: usize = 24;
