//! The core runtime services and layouts that helper libraries (`perry-std`, `perry-fs`)
//! build on.
//!
//! In linked modules these are `__perry_helper` imports that `src/link` resolves against the
//! `perry_*` exports of `perry-runtime` (and `number_format` of the number helper), and
//! canonical built-ins; on the host, the runtime crate itself and recording stand-ins provide
//! them. The crate exports nothing, so a library carries only the pieces it uses.
#![cfg_attr(target_arch = "wasm32", no_std)]

use core::ptr;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// Callback events and copy results of the canonical ABI.
pub const EVENT_SUBTASK: u32 = 1;
pub const EVENT_STREAM_READ: u32 = 2;
pub const EVENT_STREAM_WRITE: u32 = 3;
pub const EVENT_FUTURE_READ: u32 = 4;
pub const SUBTASK_RETURNED: u32 = 2;
pub const BLOCKED: u32 = 0xffff_ffff;
pub const COMPLETED: u32 = 0;
pub const DROPPED: u32 = 1;

/// `TypeError`, `RangeError` and `Error` statuses (`perry_runtime::exception`).
pub const TYPE_ERROR: u32 = 1;
pub const RANGE_ERROR: u32 = 2;
pub const ERROR: u32 = 3;
/// Exception layout (`perry_runtime::exception`): `kind` at 0, `tag` at 4, `value` at 8.
pub const EXCEPTION_KIND: usize = 0;
pub const EXCEPTION_TAG: usize = 4;
pub const EXCEPTION_VALUE: usize = 8;
pub const THROWN_VALUE: u32 = 4;
pub const TAG_NUMBER: u32 = 1;
pub const TAG_STRING: u32 = 3;
/// Bits of `undefined` in a `number | undefined` slot (`perry_codegen::UNDEFINED_BITS`).
pub const UNDEFINED_BITS: u64 = 0x7FF0_0000_0000_0001;
/// Offset of an array's data address (`perry_runtime::array::DATA`).
pub const ARRAY_DATA: u32 = 8;
/// `Uint8Array` layout (`perry_std::bytes`): byte length and data address.
pub const BYTES_LEN: usize = 0;
pub const BYTES_DATA: usize = 4;
/// String layout (`perry_runtime::string`): byte length and data address.
const STRING_LEN: usize = 0;
const STRING_DATA: usize = 8;

#[cfg(not(target_arch = "wasm32"))]
const _: () = {
    use perry_runtime::{array, exception, string};
    assert!(exception::TYPE_ERROR == TYPE_ERROR);
    assert!(exception::RANGE_ERROR == RANGE_ERROR);
    assert!(exception::ERROR == ERROR);
    assert!(exception::THROWN_VALUE == THROWN_VALUE);
    assert!(exception::TAG_NUMBER == TAG_NUMBER && exception::TAG_STRING == TAG_STRING);
    assert!(exception::KIND as usize == EXCEPTION_KIND && exception::TAG as usize == EXCEPTION_TAG);
    assert!(exception::VALUE as usize == EXCEPTION_VALUE);
    assert!(array::DATA == ARRAY_DATA);
    assert!(string::LEN as usize == STRING_LEN && string::DATA as usize == STRING_DATA);
};

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "__perry_helper")]
    unsafe extern "C" {
        pub fn perry_alloc_shaped(size: u32, shape: *const u32) -> *mut u8;
        pub fn perry_operation_pin(owner: *mut u8) -> u32;
        pub fn perry_operation_unpin(token: u32);
        pub fn perry_promise_new_typed(references: u32) -> *mut u8;
        pub fn perry_array_new_typed(capacity: u32, references: u32) -> *mut u8;
        pub fn perry_alloc(size: u32) -> *mut u8;
        pub fn perry_cabi_release(data: *mut u8);
        pub fn perry_string_offset(string: *mut u8, position: u32) -> u32;
        pub fn perry_string_view(string: *mut u8, from: u32, to: u32) -> *mut u8;
        pub fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8;
        pub fn perry_raise(kind: u32, message: *mut u8);
        pub fn perry_status() -> u32;
        pub fn perry_promise_reuse(promise: *mut u8) -> *mut u8;
        pub fn perry_task_current() -> *mut u8;
        pub fn perry_task_resume(task: *mut u8);
        pub fn perry_promise_resolve(promise: *mut u8, value: u64);
        pub fn perry_promise_reject(promise: *mut u8, status: i32);
        pub fn perry_task_join(waitable: u32);
        pub fn perry_write_acknowledged(fd: u32, data: *const u8, len: u32, promise: *mut u8);
        /// ECMAScript `Number::toString` of `value` into 40 bytes at `output` (number helper).
        pub fn number_format(value: f64, output: *mut u8) -> u32;
    }

    #[link(wasm_import_module = "$root")]
    unsafe extern "C" {
        #[link_name = "[waitable-join]"]
        pub fn waitable_join(waitable: u32, set: u32);
        #[link_name = "[subtask-drop]"]
        pub fn subtask_drop(subtask: u32);
        #[link_name = "[subtask-cancel]"]
        pub fn subtask_cancel(subtask: u32) -> u32;
    }

    #[link(wasm_import_module = "wasi:random/random@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-random-u64"]
        pub fn random_u64() -> u64;
    }

    #[link(wasm_import_module = "wasi:random/insecure@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-insecure-random-u64"]
        pub fn insecure_random_u64() -> u64;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::missing_safety_doc)]
mod imports {
    extern crate std;
    pub use perry_runtime::{
        array::perry_array_new_typed,
        exception::{perry_raise, perry_status},
        host::{subtask_cancel, subtask_drop, waitable_join},
        io::{perry_task_join, perry_write_acknowledged},
        promise::{
            perry_promise_new_typed, perry_promise_reject, perry_promise_resolve,
            perry_promise_reuse,
        },
        region::{
            perry_alloc, perry_alloc_shaped, perry_cabi_release, perry_operation_pin,
            perry_operation_unpin, perry_task_current,
        },
        string::{perry_string_offset, perry_string_view},
    };

    pub unsafe fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8 {
        unsafe { perry_runtime::string::perry_string_adopt(data, len) }
    }
    pub unsafe fn perry_task_resume(task: *mut u8) {
        unsafe { perry_runtime::region::perry_task_resume(task.cast()) };
    }
    pub unsafe fn number_format(value: f64, output: *mut u8) -> u32 {
        // Rust's shortest round-trip notation; equal to ECMAScript's for test values.
        let text = std::format!("{value}");
        unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), output, text.len()) };
        text.len() as u32
    }
    pub unsafe fn random_u64() -> u64 {
        0x0123_4567_89ab_cdef
    }
    pub unsafe fn insecure_random_u64() -> u64 {
        0xfedc_ba98_7654_3210
    }
}

/// `size` bytes in the task region, 8-aligned.
#[allow(unused_unsafe)]
pub fn alloc(size: u32) -> *mut u8 {
    // SAFETY: a runtime service without preconditions.
    unsafe { imports::perry_alloc(size) }
}

/// Release the host pin after a canonical payload is adopted or discarded.
///
/// # Safety
/// `data` was lowered through the component's canonical allocator.
pub unsafe fn release_canonical(data: *mut u8) {
    unsafe { imports::perry_cabi_release(data) };
}

/// An object with a static exact pointer map; allocation cannot trigger collection.
///
/// # Safety
/// Shape is `[stride, count, offsets...]`, valid for every instance of this allocation.
#[allow(unused_unsafe)]
pub unsafe fn alloc_shaped(size: u32, shape: &'static [u32]) -> *mut u8 {
    unsafe { imports::perry_alloc_shaped(size, shape.as_ptr()) }
}

/// Allocate a native helper object, tracing the listed pointer or Buffer fields.
#[macro_export]
macro_rules! object {
    ($ty:ty; $($($field:tt).+),* $(,)?) => {{
        const COUNT: usize = [$(stringify!($($field).+)),*].len();
        static SHAPE: [u32; 2 + COUNT] = [0, COUNT as u32, $(core::mem::offset_of!($ty, $($field).+) as u32),*];
        // SAFETY: callers list only pointer fields; Buffer has its pointer at offset zero.
        #[allow(unused_unsafe)]
        unsafe { $crate::alloc_shaped(core::mem::size_of::<$ty>() as u32, &SHAPE) as *mut $ty }
    }};
}

#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn pin(owner: *mut u8) -> u32 {
    unsafe { imports::perry_operation_pin(owner) }
}
#[allow(unused_unsafe)]
pub fn unpin(token: u32) {
    unsafe { imports::perry_operation_unpin(token) };
}

/// A string over `len` bytes of valid UTF-8 at `data`, which must outlive the task.
///
/// # Safety
/// `data` must point to `len` readable bytes of valid UTF-8.
pub unsafe fn adopt(data: *mut u8, len: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe { imports::perry_string_adopt(data, len) }
}

/// # Safety
/// `string` is live; position is a scalar index.
pub unsafe fn string_offset(string: *mut u8, position: u32) -> u32 {
    unsafe { imports::perry_string_offset(string, position) }
}
/// # Safety
/// `from..to` names valid UTF-8 boundaries within `string`.
pub unsafe fn string_view(string: *mut u8, from: u32, to: u32) -> *mut u8 {
    unsafe { imports::perry_string_view(string, from, to) }
}

/// The UTF-8 bytes of a string.
///
/// # Safety
/// `string` must be a string that outlives the returned slice's use.
pub unsafe fn bytes(string: *mut u8) -> &'static [u8] {
    // SAFETY: guaranteed by the caller; layout of `perry_runtime::string`.
    unsafe {
        let len = *(string.add(STRING_LEN) as *const u32);
        let data = *(string.add(STRING_DATA) as *const *const u8);
        core::slice::from_raw_parts(data, len as usize)
    }
}

/// A string copying `bytes` (valid UTF-8) into the task region.
pub fn from_bytes(bytes: &[u8]) -> *mut u8 {
    if bytes.is_empty() {
        return from_static("");
    }
    let data = alloc(bytes.len() as u32);
    // SAFETY: fresh block of `len` bytes, then valid UTF-8 living as long as the task.
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
        adopt(data, bytes.len() as u32)
    }
}

mod byte_buffer;
pub use byte_buffer::{
    ByteBuffer, Bytes, buffer_view, bytes_subview, bytes_transfer, bytes_view, normalize_bytes,
    view_bytes,
};

/// A string over static text.
pub fn from_static(text: &'static str) -> *mut u8 {
    // SAFETY: static UTF-8.
    unsafe { adopt(text.as_ptr() as *mut u8, text.len() as u32) }
}

/// Unicode scalar count in valid UTF-8.
pub fn scalars_length(bytes: &[u8]) -> u32 {
    bytes.iter().map(|&b| u32::from(b & 0xC0 != 0x80)).sum()
}

/// Throw an `Error` of `kind` with `message`: the exception becomes pending, and compiled
/// code finds its status with `perry_status`.
pub fn raise(kind: u32, message: &'static str) {
    raise_string(kind, from_static(message));
}

/// [raise] with a message string.
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn raise_string(kind: u32, message: *mut u8) {
    // SAFETY: a runtime service; the message is a live string.
    unsafe { imports::perry_raise(kind, message) }
}

/// ECMAScript `Number::toString` of `value` into `output`; returns the length.
pub fn number_format(value: f64, output: &mut [u8; 40]) -> usize {
    // SAFETY: the number helper writes at most 40 bytes.
    unsafe { imports::number_format(value, output.as_mut_ptr()) as usize }
}

/// A pending promise of the current task.
#[allow(unused_unsafe)]
pub fn promise_new() -> *mut u8 {
    promise_typed(true)
}

#[allow(unused_unsafe)]
pub fn promise_typed(references: bool) -> *mut u8 {
    // SAFETY: a runtime service without preconditions.
    unsafe { imports::perry_promise_new_typed(u32::from(references)) }
}

/// `promise` pending again if it settled and nothing waits on it, else a new promise.
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn promise_reuse(promise: *mut u8) -> *mut u8 {
    // SAFETY: a runtime service; a non-null argument is a promise of the current task.
    unsafe { imports::perry_promise_reuse(promise) }
}

/// The current task's identity.
#[allow(unused_unsafe)]
pub fn task() -> *mut u8 {
    // SAFETY: a runtime service without preconditions.
    unsafe { imports::perry_task_current().cast() }
}

/// Fulfil `promise` with slot bits `value`.
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn resolve(promise: *mut u8, value: u64) {
    // SAFETY: a live promise of the current task.
    unsafe { imports::perry_promise_resolve(promise, value) }
}

/// Reject `promise` with an `Error` carrying `message`.
pub fn reject(promise: *mut u8, message: *mut u8) {
    reject_with(promise, ERROR, message);
}

/// Reject `promise` with an error of `kind` (`TYPE_ERROR`, `RANGE_ERROR`, `ERROR`).
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn reject_with(promise: *mut u8, kind: u32, message: *mut u8) {
    // SAFETY: runtime services; raising leaves the exception pending for the rejection, and
    // the status is taken so no compiled status check sees it.
    unsafe {
        imports::perry_raise(kind, message);
        imports::perry_status();
        imports::perry_promise_reject(promise, kind as i32);
    }
}

/// A reference array and its writable slots; allocation cannot collect inside a helper.
#[allow(unused_unsafe)]
pub fn array_storage(len: usize) -> (*mut u8, &'static mut [u64]) {
    // SAFETY: a fresh array with capacity for `len` (layout of `perry_runtime::array`).
    unsafe {
        let len = u32::try_from(len).expect("array length overflow");
        let array = imports::perry_array_new_typed(len, 1);
        let data = *(array.add(ARRAY_DATA as usize) as *const *mut u64);
        *(array as *mut u32) = len;
        (array, core::slice::from_raw_parts_mut(data, len as usize))
    }
}

/// An array of reference slot bits, retaining their owners through the array shape.
pub fn array(values: &[u64]) -> *mut u8 {
    let (array, slots) = array_storage(values.len());
    slots.copy_from_slice(values);
    array
}

/// Append `bytes` to the task's output `fd` (0 stdout, 1 stderr); `promise` settles once the
/// host accepted them.
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn write_acknowledged(fd: u32, bytes: &[u8], promise: *mut u8) {
    // SAFETY: a runtime service over a live slice and a pending promise.
    unsafe { imports::perry_write_acknowledged(fd, bytes.as_ptr(), bytes.len() as u32, promise) }
}

/// Join `waitable` to the current task's waitable set.
#[allow(unused_unsafe)]
pub fn join(waitable: u32) {
    // SAFETY: a runtime service on a handle of this task.
    unsafe { imports::perry_task_join(waitable) }
}

/// Leave the task's waitable set, before dropping `waitable`.
pub fn unjoin(waitable: u32) {
    // SAFETY: canonical built-in on a handle of this task.
    unsafe { imports::waitable_join(waitable, 0) }
}

/// Drop a returned subtask.
pub fn subtask_drop(subtask: u32) {
    // SAFETY: canonical built-in on a returned subtask of this task.
    unsafe { imports::subtask_drop(subtask) }
}

/// Cancel an unjoined subtask and wait for acknowledgement before releasing its storage.
pub fn subtask_cancel(subtask: u32) -> u32 {
    acknowledged(|| unsafe { imports::subtask_cancel(subtask) })
}

/// Restore the current task after a canonical cancellation that can run another callback.
pub fn acknowledged<T>(cancel: impl FnOnce() -> T) -> T {
    let current = task();
    let result = cancel();
    // SAFETY: cancellation cannot release the caller's task.
    unsafe { imports::perry_task_resume(current) };
    result
}

/// 64 bits from `wasi:random/random`.
pub fn random_u64() -> u64 {
    // SAFETY: WASI import.
    unsafe { imports::random_u64() }
}

/// 64 bits from `wasi:random/insecure`.
pub fn insecure_random_u64() -> u64 {
    // SAFETY: WASI import.
    unsafe { imports::insecure_random_u64() }
}

/// A growable byte buffer in the task region: released with the task.
#[repr(C)]
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
    cap: usize,
    shape: *const u32,
}

impl Buffer {
    pub const fn new() -> Self {
        Buffer {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
            shape: ptr::null(),
        }
    }

    /// A buffer of repeated typed entries; unused capacity is zeroed before tracing.
    ///
    /// # Safety
    /// `shape` describes every entry written to this buffer, with a nonzero repetition stride.
    pub const unsafe fn with_shape(shape: &'static [u32]) -> Self {
        Self {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
            shape: shape.as_ptr(),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Make room for at least `additional` more bytes.
    pub fn reserve(&mut self, additional: usize) {
        if self.len + additional > self.cap {
            let cap = (self.cap * 2).max(self.len + additional).max(64);
            // SAFETY: every instance keeps the same static element shape.
            let ptr = unsafe { imports::perry_alloc_shaped(cap as u32, self.shape) };
            if self.len > 0 {
                // SAFETY: both blocks are live and disjoint.
                unsafe { ptr::copy_nonoverlapping(self.ptr, ptr, self.len) };
            }
            self.ptr = ptr;
            self.cap = cap;
        }
    }

    /// The reserved bytes after the contents, for a host to write into.
    pub fn spare(&mut self) -> (*mut u8, usize) {
        // SAFETY: within the block (`len <= cap`).
        (unsafe { self.ptr.add(self.len) }, self.cap - self.len)
    }

    /// Count `count` spare bytes as written.
    pub fn advance(&mut self, count: usize) {
        self.len = (self.len + count).min(self.cap);
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.reserve(bytes.len());
        // SAFETY: capacity was ensured above.
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), self.ptr.add(self.len), bytes.len()) };
        self.len += bytes.len();
    }

    pub fn push(&mut self, byte: u8) {
        self.extend_from_slice(&[byte]);
    }

    pub fn as_slice(&self) -> &[u8] {
        if self.len == 0 {
            return &[];
        }
        // SAFETY: `len` bytes at `ptr` are initialized.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}
