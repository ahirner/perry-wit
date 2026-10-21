//! Canonical built-ins and WASI functions the runtime calls.
//!
//! On wasm32 these are imports, named with the legacy mangling `wit-component` resolves
//! (`$root` built-ins, `[stream-new-0]write-via-stream` and so on). The linker forwards the
//! imports of reachable runtime functions to the linked module, so a component imports only
//! what its program uses. On other targets a recording mock stands in, for the unit tests.

/// Status returned by `stream.write` or `future.read` when the operation is pending and will deliver a completion event.
pub const BLOCKED: u32 = 0xffff_ffff;
/// Low four bits of a copy result: the reader dropped its end.
pub(crate) const DROPPED: u32 = 1;

/// Callback events.
pub(crate) const EVENT_STREAM_WRITE: u32 = 3;
pub(crate) const EVENT_FUTURE_READ: u32 = 4;
/// Subtask state after its function returned.
pub(crate) const SUBTASK_RETURNED: u32 = 2;

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "$root")]
    unsafe extern "C" {
        #[link_name = "[waitable-set-new]"]
        pub(crate) fn waitable_set_new() -> u32;
        #[link_name = "[waitable-set-drop]"]
        pub(crate) fn waitable_set_drop(set: u32);
        #[link_name = "[waitable-join]"]
        pub(crate) fn waitable_join(waitable: u32, set: u32);
        #[link_name = "[subtask-drop]"]
        pub(crate) fn subtask_drop(subtask: u32);
        #[link_name = "[subtask-cancel]"]
        pub(crate) fn subtask_cancel(subtask: u32) -> u32;
    }

    #[link(wasm_import_module = "[export]$root")]
    unsafe extern "C" {
        #[link_name = "[future-new-0]perry-internal-promise"]
        pub(crate) fn reaction_new() -> u64;
        #[link_name = "[async-lower][future-read-0]perry-internal-promise"]
        pub(crate) fn reaction_read(readable: u32, value: *mut u8) -> u32;
        #[link_name = "[async-lower][future-write-0]perry-internal-promise"]
        pub(crate) fn reaction_write(writable: u32, value: *const u8) -> u32;
        #[link_name = "[future-drop-readable-0]perry-internal-promise"]
        pub(crate) fn reaction_drop_readable(readable: u32);
        #[link_name = "[future-drop-writable-0]perry-internal-promise"]
        pub(crate) fn reaction_drop_writable(writable: u32);
        #[link_name = "[future-cancel-read-0]perry-internal-promise"]
        pub(crate) fn reaction_cancel_read(readable: u32) -> u32;
    }

    macro_rules! output {
        ($module:literal, $new:ident, $start:ident, $write:ident, $drop_writer:ident, $read:ident, $drop_future:ident, $cancel_write:ident, $cancel_read:ident) => {
            #[link(wasm_import_module = $module)]
            unsafe extern "C" {
                #[link_name = "[stream-new-0]write-via-stream"]
                pub(crate) fn $new() -> u64;
                #[link_name = "write-via-stream"]
                pub(crate) fn $start(readable: u32) -> u32;
                #[link_name = "[async-lower][stream-write-0]write-via-stream"]
                pub(crate) fn $write(writable: u32, data: *const u8, len: u32) -> u32;
                #[link_name = "[stream-drop-writable-0]write-via-stream"]
                pub(crate) fn $drop_writer(writable: u32);
                #[link_name = "[async-lower][future-read-1]write-via-stream"]
                pub(crate) fn $read(future: u32, result: *mut u8) -> u32;
                #[link_name = "[future-drop-readable-1]write-via-stream"]
                pub(crate) fn $drop_future(future: u32);
                #[link_name = "[stream-cancel-write-0]write-via-stream"]
                pub(crate) fn $cancel_write(writable: u32) -> u32;
                #[link_name = "[future-cancel-read-1]write-via-stream"]
                pub(crate) fn $cancel_read(future: u32) -> u32;
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
        stdout_drop_future,
        stdout_cancel_write,
        stdout_cancel_read
    );
    output!(
        "wasi:cli/stderr@0.3.0",
        stderr_new,
        stderr_start,
        stderr_write,
        stderr_drop_writer,
        stderr_read,
        stderr_drop_future,
        stderr_cancel_write,
        stderr_cancel_read
    );

    #[link(wasm_import_module = "wasi:cli/environment@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-environment"]
        pub(crate) fn get_environment(result: *mut u32);
    }

    #[link(wasm_import_module = "wasi:clocks/monotonic-clock@0.3.0")]
    unsafe extern "C" {
        #[link_name = "now"]
        pub(crate) fn monotonic_now() -> u64;
    }

    #[link(wasm_import_module = "wasi:clocks/system-clock@0.3.0")]
    unsafe extern "C" {
        #[link_name = "now"]
        pub(crate) fn system_now(result: *mut u64);
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use imports::*;

/// Output stream functions of `stdout` (0) or `stderr` (1). Buffers passed to `write` and
/// `read` must stay valid until the operation completes (immediately or by its event).
#[cfg(target_arch = "wasm32")]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub(crate) mod output {
    use super::imports::*;

    macro_rules! dispatch {
        ($($name:ident($($arg:ident: $ty:ty),*) $(-> $result:ty)? => $stdout:ident, $stderr:ident;)*) => {$(
            pub(crate) fn $name(fd: usize, $($arg: $ty),*) $(-> $result)? {
                // SAFETY: the caller retains buffers until the canonical operation completes.
                dispatch!(@result unsafe {
                    if fd == 0 { $stdout($($arg),*) } else { $stderr($($arg),*) }
                } $(, $result)?)
            }
        )*};
        (@result $value:expr) => {{ $value; }};
        (@result $value:expr, $result:ty) => { $value };
    }

    dispatch! {
        new() -> u64 => stdout_new, stderr_new;
        start(readable: u32) -> u32 => stdout_start, stderr_start;
        write(writable: u32, data: *const u8, len: u32) -> u32 => stdout_write, stderr_write;
        drop_writer(writable: u32) => stdout_drop_writer, stderr_drop_writer;
        read(future: u32, result: *mut u8) -> u32 => stdout_read, stderr_read;
        drop_future(future: u32) => stdout_drop_future, stderr_drop_future;
        cancel_write(writable: u32) => stdout_cancel_write, stderr_cancel_write;
        cancel_read(future: u32) => stdout_cancel_read, stderr_cancel_read;
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
    use std::vec::Vec;

    #[derive(Default)]
    pub(crate) struct Host {
        pub(crate) written: [Vec<u8>; 2],
        block_next: bool,
        pending: Option<(usize, *const u8, u32)>,
        joined: Vec<(u32, u32)>,
        pub(crate) reactions: Vec<Reaction>,
    }

    /// A promise-reaction future: which ends are still open, and whether it was written.
    pub(crate) struct Reaction {
        pub(crate) readable: bool,
        pub(crate) writable: bool,
        written: bool,
    }

    /// Reaction futures use one handle for both ends, numbered from here.
    const FIRST_REACTION: u32 = 1000;

    fn reaction(host: &mut Host, handle: u32) -> &mut Reaction {
        &mut host.reactions[(handle - FIRST_REACTION) as usize]
    }

    pub(crate) fn with<R>(access: impl FnOnce(&mut Host) -> R) -> R {
        crate::instance::native::with(|instance| access(&mut instance.host.borrow_mut()))
    }
    const LIMIT: u32 = 5;
    /// Low four bits of a copy result that completed normally.
    const COMPLETED: u32 = 0;

    pub(crate) unsafe fn waitable_set_new() -> u32 {
        77
    }
    pub(crate) unsafe fn waitable_set_drop(_: u32) {}
    pub unsafe fn waitable_join(waitable: u32, set: u32) {
        with(|host| host.joined.push((waitable, set)));
    }
    pub unsafe fn subtask_drop(_: u32) {}
    pub unsafe fn subtask_cancel(_: u32) -> u32 {
        3
    }
    pub(crate) unsafe fn reaction_new() -> u64 {
        with(|host| {
            let handle = FIRST_REACTION + host.reactions.len() as u32;
            host.reactions.push(Reaction {
                readable: true,
                writable: true,
                written: false,
            });
            u64::from(handle) | (u64::from(handle) << 32)
        })
    }
    pub(crate) unsafe fn reaction_read(_: u32, _: *mut u8) -> u32 {
        super::BLOCKED
    }
    pub(crate) unsafe fn reaction_write(handle: u32, _: *const u8) -> u32 {
        with(|host| reaction(host, handle).written = true);
        COMPLETED
    }
    pub(crate) unsafe fn reaction_drop_readable(handle: u32) {
        with(|host| reaction(host, handle).readable = false);
    }
    pub(crate) unsafe fn reaction_drop_writable(handle: u32) {
        with(|host| reaction(host, handle).writable = false);
    }
    pub(crate) unsafe fn reaction_cancel_read(_: u32) -> u32 {
        COMPLETED
    }
    /// Handles of written reactions whose reader has not consumed them yet.
    #[cfg(test)]
    pub(crate) fn completed_reactions() -> Vec<u32> {
        with(|host| {
            (FIRST_REACTION..)
                .zip(&host.reactions)
                .filter_map(|(handle, reaction)| {
                    (reaction.readable && reaction.written).then_some(handle)
                })
                .collect()
        })
    }

    pub(crate) unsafe fn get_environment(result: *mut u32) {
        unsafe {
            *result = 0;
            *result.add(1) = 0;
        }
    }
    pub(crate) unsafe fn monotonic_now() -> u64 {
        1_500_000
    }
    pub(crate) unsafe fn system_now(result: *mut u64) {
        unsafe {
            *result = 1_700_000_000;
            *(result.add(1) as *mut u32) = 5_000_000;
        }
    }

    /// Complete the blocked write and return the copy result for the event.
    pub fn complete_pending() -> Option<(u32, u32)> {
        with(|host| host.pending.take()).map(|(fd, data, len)| {
            let len = len.min(LIMIT);
            let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
            with(|host| host.written[fd].extend_from_slice(bytes));
            (10 + fd as u32 * 2 + 1, len << 4)
        })
    }

    pub mod output {
        use super::*;
        pub(crate) fn new(fd: usize) -> u64 {
            (10 + fd as u64 * 2) | ((11 + fd as u64 * 2) << 32)
        }
        pub(crate) fn start(fd: usize, _: u32) -> u32 {
            20 + fd as u32
        }
        pub fn write(fd: usize, _: u32, data: *const u8, len: u32) -> u32 {
            let block = with(|host| {
                host.block_next = !host.block_next;
                !host.block_next
            });
            if block {
                with(|host| host.pending = Some((fd, data, len)));
                return super::super::BLOCKED;
            }
            let len = len.min(LIMIT);
            let bytes = unsafe { std::slice::from_raw_parts(data, len as usize) };
            with(|host| host.written[fd].extend_from_slice(bytes));
            len << 4
        }
        pub(crate) fn drop_writer(_: usize, _: u32) {}
        pub(crate) fn read(_: usize, _: u32, _: *mut u8) -> u32 {
            COMPLETED
        }
        pub(crate) fn drop_future(_: usize, _: u32) {}
        pub(crate) fn cancel_write(_: usize, _: u32) {
            with(|host| host.pending = None);
        }
        pub(crate) fn cancel_read(_: usize, _: u32) {}
    }
}
