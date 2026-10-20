//! The core runtime services and layouts that helper libraries (`perry-std`, `perry-fs`)
//! build on.
//!
//! In linked modules these are `__perry_helper` imports that `src/link` resolves against the
//! `perry_*` exports of `perry-runtime` (and `number_format` of the number helper), and
//! canonical built-ins; on the host, the runtime crate itself and recording stand-ins provide
//! them. The crate exports nothing, so a library carries only the pieces it uses.
#![cfg_attr(target_arch = "wasm32", no_std)]

use core::ptr;

mod wasm_imports;

pub mod json;
pub mod layout;
pub mod number;
// Public because `perry-runtime` compiles this source too and uses the compact counter.
pub mod utf8;
pub mod whitespace;
/// Unicode scalar count in valid UTF-8, scanning eight bytes at a time.
pub use utf8::scalars_length_wide as scalars_length;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
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

/// `TypeError`, `RangeError`, `Error`, `ReferenceError` and `SyntaxError` statuses and layout.
pub use layout::exception::{
    ERROR, RANGE_ERROR, REFERENCE_ERROR, SYNTAX_ERROR, TAG_NUMBER, TAG_STRING, THROWN_VALUE,
    TYPE_ERROR,
};
pub const EXCEPTION_KIND: usize = layout::exception::KIND as usize;
pub const EXCEPTION_TAG: usize = layout::exception::TAG as usize;
pub const EXCEPTION_VALUE: usize = layout::exception::VALUE as usize;
/// Bits of an omitted optional field in a `number | undefined` slot.
pub use layout::descriptor::OMITTED_BITS;
/// Bits of `undefined` in a `number | undefined` slot: a signalling NaN that Wasm arithmetic
/// never produces. `perry-runtime`, which has no dependencies, keeps its own copy.
pub use layout::descriptor::UNDEFINED_BITS;
/// Offset of an array's data address ([layout::array::DATA]).
pub const ARRAY_DATA: u32 = layout::array::DATA;
/// Offset of a `Uint8Array`'s data address in the wasm32 layout of [Bytes].
pub const BYTES_DATA: usize = 4;

#[allow(clippy::missing_safety_doc)]
mod imports {
    crate::wasm_imports!("__perry_helper" => perry_runtime::region;
        fn perry_alloc(size: u32) -> *mut u8;
        fn perry_alloc_shaped(size: u32, shape: *const u32) -> *mut u8;
        fn perry_buffer_resize(
            old: *mut u8,
            old_size: u32,
            new_size: u32,
            initialized: u32,
            shape: *const u32,
        ) -> *mut u8;
        fn perry_cabi_release(data: *mut u8);
        fn perry_operation_pin(owner: *mut u8) -> u32;
        fn perry_operation_unpin(token: u32);
        fn perry_task_current() -> *mut u8;
    );
    crate::wasm_imports!("__perry_helper" => perry_runtime::string;
        fn perry_string_adopt_counted(data: *mut u8, len: u32, scalars: u32) -> *mut u8;
        fn perry_string_offset(string: *mut u8, position: u32) -> u32;
        fn perry_string_view(string: *mut u8, from: u32, to: u32) -> *mut u8;
    );
    crate::wasm_imports!("__perry_helper" => perry_runtime::exception;
        fn perry_exception_message(exception: *mut u8) -> *mut u8;
        fn perry_exception_new(kind: u32, tag: u32, value: u64) -> *mut u8;
        fn perry_raise(kind: u32, message: *mut u8);
        fn perry_raise_with_cause(kind: u32, message: *mut u8, cause: *mut u8);
        fn perry_status() -> u32;
    );
    crate::wasm_imports!("__perry_helper" => perry_runtime::promise;
        fn perry_promise_cancellation_owned(promise: *mut u8) -> u32;
        fn perry_promise_new_typed(references: u32) -> *mut u8;
        fn perry_promise_reject(promise: *mut u8, status: i32);
        fn perry_promise_resolve(promise: *mut u8, value: u64);
        fn perry_promise_reuse(promise: *mut u8) -> *mut u8;
    );
    crate::wasm_imports!("__perry_helper" => perry_runtime::array;
        fn perry_array_new_typed(capacity: u32, references: u32) -> *mut u8;
    );
    crate::wasm_imports!("__perry_helper" => perry_runtime::io;
        fn perry_task_join(waitable: u32);
        fn perry_write_acknowledged(fd: u32, data: *const u8, len: u32, promise: *mut u8);
    );

    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "__perry_helper")]
    unsafe extern "C" {
        pub(super) fn perry_task_resume(task: *mut u8);
        pub(super) fn number_format(value: f64, output: *mut u8) -> u32;
    }
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "$root")]
    unsafe extern "C" {
        #[link_name = "[waitable-join]"]
        pub(super) fn waitable_join(waitable: u32, set: u32);
        #[link_name = "[subtask-drop]"]
        pub(super) fn subtask_drop(subtask: u32);
        #[link_name = "[subtask-cancel]"]
        pub(super) fn subtask_cancel(subtask: u32) -> u32;
    }
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "wasi:random/random@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-random-u64"]
        pub(super) fn random_u64() -> u64;
    }
    #[cfg(target_arch = "wasm32")]
    #[link(wasm_import_module = "wasi:random/insecure@0.3.0")]
    unsafe extern "C" {
        #[link_name = "get-insecure-random-u64"]
        pub(super) fn insecure_random_u64() -> u64;
    }

    #[cfg(not(target_arch = "wasm32"))]
    extern crate std;
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) use perry_runtime::host::{subtask_cancel, subtask_drop, waitable_join};
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) unsafe fn perry_task_resume(task: *mut u8) {
        unsafe { perry_runtime::region::perry_task_resume(task.cast()) };
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) unsafe fn number_format(value: f64, output: *mut u8) -> u32 {
        let text = std::format!("{value}");
        unsafe { core::ptr::copy_nonoverlapping(text.as_ptr(), output, text.len()) };
        text.len() as u32
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) unsafe fn random_u64() -> u64 {
        0x0123_4567_89ab_cdef
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) unsafe fn insecure_random_u64() -> u64 {
        0xfedc_ba98_7654_3210
    }
}

/// `size` bytes in the task region, 8-aligned.
#[allow(unused_unsafe)]
pub fn alloc(size: u32) -> *mut u8 {
    // SAFETY: a runtime service without preconditions.
    unsafe { imports::perry_alloc(size) }
}

/// `len` fresh, uninitialized bytes in the task region, exclusively the caller's.
///
/// Like every task allocation, the block lives until the task ends; `'static` stands for that.
fn alloc_uninit(len: usize) -> &'static mut [core::mem::MaybeUninit<u8>] {
    let len32 = u32::try_from(len).expect("allocation size overflow");
    // SAFETY: a fresh, 8-aligned block of `len` bytes that nothing else references; any
    // byte pattern (including none) is a valid `MaybeUninit<u8>`.
    unsafe { core::slice::from_raw_parts_mut(alloc(len32.max(1)).cast(), len) }
}

/// Release the host pin after a canonical payload is adopted or discarded.
///
/// # Safety
/// `data` was lowered through the component's canonical allocator.
pub unsafe fn release_canonical(data: *mut u8) {
    unsafe { imports::perry_cabi_release(data) };
}

/// An object with a static exact pointer map; allocation cannot trigger collection.
/// Collectible storage is zeroed; task-region storage must be initialized by the caller.
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
#[inline(never)]
pub unsafe fn adopt(data: *mut u8, len: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let bytes = core::slice::from_raw_parts(data, len as usize);
        adopt_counted(data, len, scalars_length(bytes))
    }
}

/// Adopt immutable UTF-8 with the scalar count established by its producer.
///
/// # Safety
/// `data` contains `len` readable UTF-8 bytes with exactly `scalars` scalars and outlives the task.
pub unsafe fn adopt_counted(data: *mut u8, len: u32, scalars: u32) -> *mut u8 {
    unsafe { imports::perry_string_adopt_counted(data, len, scalars) }
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
        let header = &*string.cast::<layout::string::Header>();
        if header.len == 0 {
            return &[];
        }
        core::slice::from_raw_parts(header.data, header.len as usize)
    }
}

/// A string copying `bytes` (valid UTF-8) into the task region.
pub fn from_bytes(bytes: &[u8]) -> *mut u8 {
    if bytes.is_empty() {
        return from_static("");
    }
    let data = alloc_uninit(bytes.len()).write_copy_of_slice(bytes);
    // SAFETY: copied UTF-8 in task storage; the string takes ownership of the allocation.
    unsafe { adopt(data.as_mut_ptr(), data.len() as u32) }
}

/// Decode `bytes` in task storage as UTF-8, replacing ill-formed sequences with U+FFFD and
/// optionally stripping a leading UTF-8 BOM.
pub fn from_utf8_lossy(bytes: &[u8], strip_bom: bool) -> *mut u8 {
    let bytes = if strip_bom {
        bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes)
    } else {
        bytes
    };
    if core::str::from_utf8(bytes).is_ok() {
        // SAFETY: valid UTF-8 in task storage.
        return unsafe { adopt(bytes.as_ptr() as *mut u8, bytes.len() as u32) };
    }
    let mut out = Buffer::new();
    out.reserve(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.extend_from_slice(chunk.valid().as_bytes());
        if !chunk.invalid().is_empty() {
            out.extend_from_slice("\u{FFFD}".as_bytes());
        }
    }
    unsafe { out.into_string() }
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

/// Take and clear the pending runtime status when returning it directly to compiled code.
#[allow(unused_unsafe)]
pub fn take_status() -> u32 {
    // SAFETY: the runtime owns the current task's status.
    unsafe { imports::perry_status() }
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

/// Whether task cancellation assigned this promise's work to a completed combinator.
#[allow(unused_unsafe, clippy::not_unsafe_ptr_arg_deref)]
pub fn cancellation_owned(promise: *mut u8) -> bool {
    // SAFETY: runtime services pass null or a live promise of the current task.
    unsafe { imports::perry_promise_cancellation_owned(promise) != 0 }
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
    array_storage_typed(len, true)
}

/// Typed array slots whose pointer map follows their representation.
#[allow(unused_unsafe)]
pub fn array_storage_typed(len: usize, references: bool) -> (*mut u8, &'static mut [u64]) {
    // SAFETY: a fresh array with capacity for `len` (layout of `perry_runtime::array`).
    unsafe {
        let len = u32::try_from(len).expect("array length overflow");
        let array = imports::perry_array_new_typed(len, u32::from(references));
        let data = *(array.add(ARRAY_DATA as usize) as *const *mut u64);
        *(array as *mut u32) = len;
        (
            array,
            if len == 0 {
                &mut []
            } else {
                core::slice::from_raw_parts_mut(data, len as usize)
            },
        )
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

/// Exclusive growable storage in the module's selected allocation tier.
/// Exposed immutable views require finishing mutation; host borrows must acknowledge before reserve.
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
    #[inline(never)]
    pub fn reserve(&mut self, additional: usize) {
        let required = self
            .len
            .checked_add(additional)
            .expect("buffer capacity overflow");
        if required <= self.cap {
            return;
        }
        let cap = self.cap.saturating_mul(2).max(required).max(64);
        let new_size = u32::try_from(cap).expect("buffer capacity overflow");
        // SAFETY: Buffer owns this payload exclusively; transfer callers reserve only after
        // acknowledgement. The fixed shape describes both zero-initialized capacities.
        self.ptr = unsafe {
            imports::perry_buffer_resize(
                self.ptr,
                self.cap as u32,
                new_size,
                self.len as u32,
                self.shape,
            )
        };
        self.cap = cap;
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

    #[inline(never)]
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

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: `len` bytes are initialized and the buffer owns its payload exclusively.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// Transfer ownership of valid UTF-8 bytes in this buffer into a runtime string.
    ///
    /// # Safety
    /// `self` must contain `self.len` bytes of valid UTF-8.
    pub unsafe fn into_string(self) -> *mut u8 {
        if self.len == 0 {
            return from_static("");
        }
        unsafe { adopt(self.ptr, self.len as u32) }
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new()
    }
}

// Here rather than in `utf8.rs`, which `perry-runtime` also compiles: the test runs once.
/// Allocate the strict JSON parser's message from its original byte coordinates.
pub fn json_syntax_message(input: &[u8], offset: u32) -> *mut u8 {
    let mut message = Buffer::new();
    json::syntax_message(input, offset, |part| message.extend_from_slice(part));
    // SAFETY: the buffer owns valid UTF-8 and transfers its payload to the string.
    unsafe { message.into_string() }
}

/// The message of a live exception supplied by the compiler or another helper.
#[allow(clippy::not_unsafe_ptr_arg_deref, unused_unsafe)]
pub fn exception_message(exception: *mut u8) -> *mut u8 {
    unsafe { imports::perry_exception_message(exception) }
}

/// Box a supported thrown value, retaining reference payloads through their runtime tag.
#[allow(unused_unsafe)]
pub fn exception_value(tag: u32, value: u64) -> *mut u8 {
    // SAFETY: helper callers select the payload representation named by tag.
    unsafe { imports::perry_exception_new(THROWN_VALUE, tag, value) }
}

/// Reject a native operation with category, code and its original path or URL.
#[allow(clippy::not_unsafe_ptr_arg_deref, unused_unsafe)]
#[inline(never)]
pub fn reject_native(
    promise: *mut u8,
    kind: u32,
    message: *mut u8,
    code: &str,
    category: &str,
    location_key: &str,
    location: &[u8],
) {
    #[inline(never)]
    fn string(bytes: &[u8]) -> *mut u8 {
        let data = alloc(bytes.len() as u32);
        let node = json::new_node(json::kind::STRING);
        unsafe {
            ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            json::set_word(node, 8, data as usize as u32);
            json::set_word(node, 12, bytes.len() as u32);
            json::set_word(node, 16, scalars_length(bytes));
        }
        node
    }
    let cause = json::new_node(json::kind::OBJECT);
    for (key, value) in [
        ("code", code.as_bytes()),
        ("category", category.as_bytes()),
        (location_key, location),
    ] {
        let member = string(value);
        let key = json::address(string(key.as_bytes()));
        unsafe {
            json::set_word(member, json::KEY, key);
            json::link_child(cause, member);
        }
    }
    unsafe {
        imports::perry_raise_with_cause(kind, message, cause);
        imports::perry_status();
        imports::perry_promise_reject(promise, kind as i32);
    }
}

#[cfg(test)]
mod tests {
    use crate::utf8::{scalars_length, scalars_length_wide};

    #[test]
    fn scalar_counts_cross_word_boundaries() {
        for prefix in 0..24 {
            for suffix in 0..24 {
                let text = format!("{}é漢🙂\0{}", "a".repeat(prefix), "z".repeat(suffix));
                assert_eq!(
                    scalars_length(text.as_bytes()) as usize,
                    text.chars().count()
                );
                assert_eq!(
                    scalars_length_wide(text.as_bytes()),
                    scalars_length(text.as_bytes())
                );
            }
        }
        assert_eq!(scalars_length(b""), 0);
        assert_eq!(scalars_length_wide(b""), 0);
    }
}
