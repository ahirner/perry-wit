//! Canonical built-ins and WASI functions the runtime calls.
//!
//! On wasm32 these are imports, named with the legacy mangling `wit-component` resolves
//! (`$root` built-ins, `[stream-new-0]write-via-stream` and so on). `src/link` forwards the
//! imports of reachable runtime functions to the linked module, so a component imports only
//! what its program uses. On other targets a recording mock stands in, for the unit tests.

/// `stream.write` / `future.read` result: the operation is pending; an event follows.
pub const BLOCKED: u32 = 0xffff_ffff;
/// Low four bits of a copy result.
pub const COMPLETED: u32 = 0;
pub const DROPPED: u32 = 1;

/// Callback events.
pub const EVENT_SUBTASK: u32 = 1;
pub const EVENT_STREAM_WRITE: u32 = 3;
pub const EVENT_FUTURE_READ: u32 = 4;
/// Subtask state after its function returned.
pub const SUBTASK_RETURNED: u32 = 2;

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "$root")]
    unsafe extern "C" {
        #[link_name = "[waitable-set-new]"]
        pub fn waitable_set_new() -> u32;
        #[link_name = "[waitable-set-drop]"]
        pub fn waitable_set_drop(set: u32);
        #[link_name = "[waitable-join]"]
        pub fn waitable_join(waitable: u32, set: u32);
        #[link_name = "[subtask-drop]"]
        pub fn subtask_drop(subtask: u32);
    }

    macro_rules! output {
        ($module:literal, $new:ident, $start:ident, $write:ident, $drop_writer:ident, $read:ident, $drop_future:ident) => {
            #[link(wasm_import_module = $module)]
            unsafe extern "C" {
                #[link_name = "[stream-new-0]write-via-stream"]
                pub fn $new() -> u64;
                #[link_name = "write-via-stream"]
                pub fn $start(readable: u32) -> u32;
                #[link_name = "[async-lower][stream-write-0]write-via-stream"]
                pub fn $write(writable: u32, data: *const u8, len: u32) -> u32;
                #[link_name = "[stream-drop-writable-0]write-via-stream"]
                pub fn $drop_writer(writable: u32);
                #[link_name = "[async-lower][future-read-1]write-via-stream"]
                pub fn $read(future: u32, result: *mut u8) -> u32;
                #[link_name = "[future-drop-readable-1]write-via-stream"]
                pub fn $drop_future(future: u32);
            }
        };
    }
    output!(
        "wasi:cli/stdout@0.3.0",
        stdout_new,
        stdout_start,
        stdout_write,
        stdout_drop_writer,
        stdout_read,
        stdout_drop_future
    );
    output!(
        "wasi:cli/stderr@0.3.0",
        stderr_new,
        stderr_start,
        stderr_write,
        stderr_drop_writer,
        stderr_read,
        stderr_drop_future
    );

    #[link(wasm_import_module = "wasi:cli/environment@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-environment"]
        pub fn get_environment(result: *mut u32);
    }

    #[link(wasm_import_module = "wasi:clocks/monotonic-clock@0.3.0")]
    unsafe extern "C" {
        #[link_name = "now"]
        pub fn monotonic_now() -> u64;
    }

    #[link(wasm_import_module = "wasi:clocks/system-clock@0.3.0")]
    unsafe extern "C" {
        #[link_name = "now"]
        pub fn system_now(result: *mut u64);
    }
}

#[cfg(target_arch = "wasm32")]
pub use imports::*;

/// Output stream functions of `stdout` (0) or `stderr` (1). Buffers passed to `write` and
/// `read` must stay valid until the operation completes (immediately or by its event).
#[cfg(target_arch = "wasm32")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub mod output {
    use super::imports::*;

    pub fn new(fd: usize) -> u64 {
        // SAFETY: canonical built-ins of the imported interface.
        unsafe { if fd == 0 { stdout_new() } else { stderr_new() } }
    }
    pub fn start(fd: usize, readable: u32) -> u32 {
        unsafe {
            if fd == 0 {
                stdout_start(readable)
            } else {
                stderr_start(readable)
            }
        }
    }
    pub fn write(fd: usize, writable: u32, data: *const u8, len: u32) -> u32 {
        unsafe {
            if fd == 0 {
                stdout_write(writable, data, len)
            } else {
                stderr_write(writable, data, len)
            }
        }
    }
    pub fn drop_writer(fd: usize, writable: u32) {
        unsafe {
            if fd == 0 {
                stdout_drop_writer(writable)
            } else {
                stderr_drop_writer(writable)
            }
        }
    }
    pub fn read(fd: usize, future: u32, result: *mut u8) -> u32 {
        unsafe {
            if fd == 0 {
                stdout_read(future, result)
            } else {
                stderr_read(future, result)
            }
        }
    }
    pub fn drop_future(fd: usize, future: u32) {
        unsafe {
            if fd == 0 {
                stdout_drop_future(future)
            } else {
                stderr_drop_future(future)
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use mock::*;

/// A host stand-in: writes complete at most `mock::LIMIT` bytes at a time, alternately
/// blocking, and record what arrived per output.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc, clippy::not_unsafe_ptr_arg_deref)]
pub mod mock {
    extern crate std;
    use std::{cell::RefCell, vec::Vec};

    std::thread_local! {
        pub static WRITTEN: RefCell<[Vec<u8>; 2]> = const { RefCell::new([Vec::new(), Vec::new()]) };
        pub static BLOCK_NEXT: RefCell<bool> = const { RefCell::new(false) };
        pub static PENDING: RefCell<Option<(usize, *const u8, u32)>> = const { RefCell::new(None) };
        pub static JOINED: RefCell<Vec<(u32, u32)>> = const { RefCell::new(Vec::new()) };
    }
    pub const LIMIT: u32 = 5;

    pub unsafe fn waitable_set_new() -> u32 {
        77
    }
    pub unsafe fn waitable_set_drop(_: u32) {}
    pub unsafe fn waitable_join(waitable: u32, set: u32) {
        JOINED.with(|j| j.borrow_mut().push((waitable, set)));
    }
    pub unsafe fn subtask_drop(_: u32) {}
    pub unsafe fn get_environment(result: *mut u32) {
        unsafe {
            *result = 0;
            *result.add(1) = 0;
        }
    }
    pub unsafe fn monotonic_now() -> u64 {
        1_500_000
    }
    pub unsafe fn system_now(result: *mut u64) {
        unsafe {
            *result = 1_700_000_000;
            *(result.add(1) as *mut u32) = 5_000_000;
        }
    }

    /// Complete the blocked write; returns the copy result for the event.
    pub fn complete_pending() -> Option<(u32, u32)> {
        PENDING
            .with(|p| p.borrow_mut().take())
            .map(|(fd, data, len)| {
                let len = len.min(LIMIT);
                let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
                WRITTEN.with(|w| w.borrow_mut()[fd].extend_from_slice(bytes));
                (10 + fd as u32 * 2 + 1, len << 4)
            })
    }

    pub mod output {
        use super::*;
        pub fn new(fd: usize) -> u64 {
            (10 + fd as u64 * 2) | ((11 + fd as u64 * 2) << 32)
        }
        pub fn start(fd: usize, _: u32) -> u32 {
            20 + fd as u32
        }
        pub fn write(fd: usize, _: u32, data: *const u8, len: u32) -> u32 {
            let block = BLOCK_NEXT.with(|b| {
                let mut b = b.borrow_mut();
                *b = !*b;
                !*b
            });
            if block {
                PENDING.with(|p| *p.borrow_mut() = Some((fd, data, len)));
                return super::super::BLOCKED;
            }
            let len = len.min(LIMIT);
            let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
            WRITTEN.with(|w| w.borrow_mut()[fd].extend_from_slice(bytes));
            len << 4
        }
        pub fn drop_writer(_: usize, _: u32) {}
        pub fn read(_: usize, _: u32, _: *mut u8) -> u32 {
            super::super::COMPLETED
        }
        pub fn drop_future(_: usize, _: u32) {}
    }
}
