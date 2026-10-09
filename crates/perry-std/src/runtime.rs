//! The core runtime services and layouts this library builds on.
//!
//! In linked modules these are `__perry_helper` imports that `src/link` resolves against the
//! `perry_*` exports of `perry-runtime` (and `number_format` of the number helper); on the
//! host, the runtime crate itself and recording stand-ins provide them.
use core::ptr;

/// `TypeError`, `RangeError` and `Error` statuses (`perry_runtime::exception`).
pub const TYPE_ERROR: u32 = 1;
pub const RANGE_ERROR: u32 = 2;
pub const ERROR: u32 = 3;
/// Bits of `undefined` in a `number | undefined` slot (`perry_codegen::UNDEFINED_BITS`).
pub const UNDEFINED_BITS: u64 = 0x7FF0_0000_0000_0001;
/// Offset of an array's data address (`perry_runtime::array::DATA`).
pub const ARRAY_DATA: u32 = 8;
/// String layout (`perry_runtime::string`): byte length and data address.
const STRING_LEN: usize = 0;
const STRING_DATA: usize = 8;

#[cfg(not(target_arch = "wasm32"))]
const _: () = {
    use perry_runtime::{array, exception, string};
    assert!(exception::TYPE_ERROR == TYPE_ERROR);
    assert!(exception::RANGE_ERROR == RANGE_ERROR);
    assert!(exception::ERROR == ERROR);
    assert!(array::DATA == ARRAY_DATA);
    assert!(string::LEN as usize == STRING_LEN && string::DATA as usize == STRING_DATA);
};

#[cfg(target_arch = "wasm32")]
mod imports {
    #[link(wasm_import_module = "__perry_helper")]
    unsafe extern "C" {
        pub fn perry_alloc(size: u32) -> *mut u8;
        pub fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8;
        pub fn perry_raise(kind: u32, message: *mut u8);
        /// ECMAScript `Number::toString` of `value` into 40 bytes at `output` (number helper).
        pub fn number_format(value: f64, output: *mut u8) -> u32;
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
    pub use perry_runtime::{exception::perry_raise, region::perry_alloc};

    pub unsafe fn perry_string_adopt(data: *mut u8, len: u32) -> *mut u8 {
        unsafe { perry_runtime::string::perry_string_adopt(data, len) }
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

/// A string over `len` bytes of valid UTF-8 at `data`, which must outlive the task.
///
/// # Safety
/// `data` must point to `len` readable bytes of valid UTF-8.
pub unsafe fn adopt(data: *mut u8, len: u32) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe { imports::perry_string_adopt(data, len) }
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

/// A string over static text.
pub fn from_static(text: &'static str) -> *mut u8 {
    // SAFETY: static UTF-8.
    unsafe { adopt(text.as_ptr() as *mut u8, text.len() as u32) }
}

/// UTF-16 code units in valid UTF-8: one per scalar, two for scalars above U+FFFF.
pub fn utf16_length(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|&b| u32::from(b & 0xC0 != 0x80) + u32::from(b >= 0xF0))
        .sum()
}

/// Throw an `Error` of `kind` with `message`: the exception becomes pending, and compiled
/// code finds its status with `perry_status`.
#[allow(unused_unsafe)]
pub fn raise(kind: u32, message: &'static str) {
    // SAFETY: a runtime service; the message is a live string.
    unsafe { imports::perry_raise(kind, from_static(message)) }
}

/// ECMAScript `Number::toString` of `value` into `output`; returns the length.
pub fn number_format(value: f64, output: &mut [u8; 40]) -> usize {
    // SAFETY: the number helper writes at most 40 bytes.
    unsafe { imports::number_format(value, output.as_mut_ptr()) as usize }
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
pub struct Buffer {
    ptr: *mut u8,
    len: usize,
    cap: usize,
}

impl Buffer {
    pub const fn new() -> Self {
        Buffer {
            ptr: ptr::null_mut(),
            len: 0,
            cap: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        if self.len + bytes.len() > self.cap {
            let cap = (self.cap * 2).max(self.len + bytes.len()).max(64);
            let ptr = alloc(cap as u32);
            if self.len > 0 {
                // SAFETY: both blocks are live and disjoint.
                unsafe { ptr::copy_nonoverlapping(self.ptr, ptr, self.len) };
            }
            self.ptr = ptr;
            self.cap = cap;
        }
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
