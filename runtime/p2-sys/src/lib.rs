//! Shared ABI of the P2 event loop and its independently linked I/O backends.
#![no_std]
extern crate alloc;

use alloc::boxed::Box;
#[cfg(target_arch = "wasm32")]
use core::alloc::{GlobalAlloc, Layout};
pub use perry_rt_sys::{
    BLOCKED, DROPPED, EVENT_FUTURE_READ, EVENT_STREAM_READ, EVENT_STREAM_WRITE, EVENT_SUBTASK,
};
pub use wasip2;

// Canonical buffers are pinned independently of guest GC roots. Rust-owned backend
// allocations therefore survive guest safe points until their owning Rust value drops.
#[cfg(target_arch = "wasm32")]
struct Allocator;
#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
#[cfg(target_arch = "wasm32")]
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        assert!(layout.align() <= 8);
        unsafe {
            cabi_realloc(
                core::ptr::null_mut(),
                0,
                layout.align() as u32,
                layout.size() as u32,
            )
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, _: Layout) {
        unsafe { perry_rt_sys::release_canonical(ptr) };
    }
}

perry_rt_sys::wasm_imports! { "__perry_helper";
    #[link_name = "perry_cabi_realloc"]
    pub fn cabi_realloc(old: *mut u8, old_size: u32, align: u32, size: u32) -> *mut u8;
    #[link_name = "perry_task_resume"]
    pub fn task_resume(task: *mut u8);
    #[link_name = "perry_task_suspend"]
    pub fn task_suspend() -> u32;
    #[link_name = "perry_io_close"]
    pub fn io_close() -> u32;
    #[link_name = "perry_io_event"]
    pub fn io_event(event: u32, handle: u32, code: u32) -> u32;
    pub fn p2_register(op: *mut u8) -> u32;
    pub fn p2_get(handle: u32) -> *mut u8;
    pub fn p2_drop(handle: u32);
    pub fn p2_poll(set: u32) -> *const u32;
    pub fn p2_context_get() -> u32;
}

/// A pending native operation. Native resources stay in a backend-specific payload,
/// preventing the scheduler from linking HTTP or filesystem imports into other programs.
pub struct Operation {
    pub set: u32,
    pub ready: bool,
    pub notified: bool,
    pub event: u32,
    pub code: u32,
    pub refs: u32,
    pub result: *mut u8,
    pub result_size: usize,
    pub error: Option<u8>,
    pub error_offset: usize,
    pub wait: Option<wasip2::io::poll::Pollable>,
    pub step: Option<unsafe extern "C" fn(*mut Operation)>,
    pub destroy: Option<unsafe extern "C" fn(*mut u8)>,
    pub payload: *mut u8,
}

impl Operation {
    pub fn new(event: u32, code: u32) -> Self {
        Self {
            set: 0,
            ready: false,
            notified: false,
            event,
            code,
            refs: 1,
            result: core::ptr::null_mut(),
            result_size: 0,
            error: None,
            error_offset: 8,
            wait: None,
            step: None,
            destroy: None,
            payload: core::ptr::null_mut(),
        }
    }

    pub fn with<T>(mut self, value: T, step: Option<unsafe extern "C" fn(*mut Operation)>) -> Self {
        unsafe extern "C" fn destroy<T>(ptr: *mut u8) {
            unsafe { drop(Box::from_raw(ptr.cast::<T>())) };
        }
        self.payload = Box::into_raw(Box::new(value)).cast();
        self.destroy = Some(destroy::<T>);
        self.step = step;
        self
    }

    pub fn register(self) -> u32 {
        unsafe { p2_register(Box::into_raw(Box::new(self)).cast()) }
    }
}

impl Drop for Operation {
    fn drop(&mut self) {
        // A pollable is a child resource and must precede its parent's destructor.
        drop(self.wait.take());
        if let Some(destroy) = self.destroy.take() {
            unsafe { destroy(self.payload) };
        }
    }
}

/// # Safety
/// `handle` is live in this instance; no other reference to its operation may be used
/// while the returned borrow is active. The reference expires when the handle is dropped.
pub unsafe fn op(handle: u32) -> &'static mut Operation {
    unsafe { &mut *p2_get(handle).cast::<Operation>() }
}

/// # Safety
/// The operation was constructed with a live payload of exactly `T`. The returned
/// reference must not alias another borrow or outlive that payload.
pub unsafe fn payload<T>(operation: &mut Operation) -> &'static mut T {
    unsafe { &mut *operation.payload.cast::<T>() }
}

/// Zero-result future used for stream completion acknowledgements.
pub fn completion(size: usize, ready: bool) -> u32 {
    let mut op = Operation::new(EVENT_FUTURE_READ, 0);
    op.result_size = size;
    op.ready = ready;
    op.register()
}

/// # Safety
/// A non-null result pointer must address `result_size` writable bytes, including
/// the error discriminants, and remain live until this operation completes.
pub unsafe fn finish(operation: &mut Operation) {
    if !operation.result.is_null() {
        unsafe { core::ptr::write_bytes(operation.result, 0, operation.result_size) };
        if let Some(code) = operation.error {
            unsafe {
                *operation.result = 1;
                *operation.result.add(operation.error_offset) = code;
            }
        }
    }
    operation.ready = true;
    drop(operation.wait.take());
}

/// # Safety
/// For nonzero `len`, the pointer addresses that many readable bytes for `'a`.
pub unsafe fn bytes<'a>(ptr: *const u8, len: u32) -> &'a [u8] {
    if len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(ptr, len as usize) }
    }
}

/// # Safety
/// The byte range satisfies [`bytes`]'s contract and contains valid UTF-8.
pub unsafe fn text<'a>(ptr: *const u8, len: u32) -> &'a str {
    core::str::from_utf8(unsafe { bytes(ptr, len) }).expect("valid UTF-8")
}
