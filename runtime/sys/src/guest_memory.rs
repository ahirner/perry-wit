//! Checked byte ranges shared by the JSON and time codec adapters.

use core::slice;

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
pub struct GuestRange {
    pointer: usize,
    pub length: usize,
}

#[cfg(target_arch = "wasm32")]
impl GuestRange {
    pub fn new(pointer: u32, length: u32) -> Option<Self> {
        let end = u64::from(pointer) + u64::from(length);
        let memory_bytes = (core::arch::wasm32::memory_size::<0>() as u64) * 65_536;
        if (pointer == 0 && length != 0)
            || length > isize::MAX as u32
            || end > memory_bytes
            || end > u64::from(u32::MAX)
        {
            return None;
        }
        Some(Self {
            pointer: pointer as usize,
            length: length as usize,
        })
    }

    pub fn overlaps(self, other: Self) -> bool {
        self.length != 0
            && other.length != 0
            && self.pointer < other.pointer + other.length
            && other.pointer < self.pointer + self.length
    }

    /// # Safety
    /// The caller must guarantee initialized, immutable storage for the borrow.
    pub unsafe fn bytes<'a>(self) -> &'a [u8] {
        if self.length == 0 {
            return &[];
        }
        // SAFETY: construction checks nonnull, bounds, and isize limits; caller supplies ownership.
        unsafe { slice::from_raw_parts(self.pointer as *const u8, self.length) }
    }

    /// # Safety
    /// The caller must guarantee exclusive ownership for the returned borrow.
    pub unsafe fn bytes_mut<'a>(self) -> &'a mut [u8] {
        if self.length == 0 {
            return &mut [];
        }
        // SAFETY: construction checks nonnull, bounds, and isize limits; caller supplies exclusivity.
        unsafe { slice::from_raw_parts_mut(self.pointer as *mut u8, self.length) }
    }
}

/// # Safety
/// `ptr` points to `byte_len` initialized UTF-8 bytes valid for the returned borrow.
pub unsafe fn str_from_raw<'a>(ptr: u32, byte_len: u32) -> &'a str {
    unsafe {
        core::str::from_utf8_unchecked(slice::from_raw_parts(ptr as *const u8, byte_len as usize))
    }
}

pub struct OutSink {
    pub out_ptr: u32,
    pub byte_len: u32,
    pub scalar_len: u32,
    pub overflow: bool,
}

impl OutSink {
    pub fn new(out_ptr: u32) -> Self {
        Self {
            out_ptr,
            byte_len: 0,
            scalar_len: 0,
            overflow: false,
        }
    }

    pub fn emit(&mut self, ch: char) -> bool {
        if self.overflow {
            return false;
        }
        let width = ch.len_utf8() as u32;
        let Some(next_len) = self.byte_len.checked_add(width) else {
            self.overflow = true;
            return false;
        };
        if self.out_ptr != 0 {
            let Some(dest) = self.out_ptr.checked_add(self.byte_len) else {
                self.overflow = true;
                return false;
            };
            // SAFETY: measurement reserved the complete output, including this scalar.
            let output = unsafe { slice::from_raw_parts_mut(dest as *mut u8, width as usize) };
            ch.encode_utf8(output);
        }
        self.byte_len = next_len;
        self.scalar_len += 1;
        true
    }

    pub fn finish(self) -> u64 {
        if self.overflow {
            u64::MAX
        } else {
            u64::from(self.byte_len) | (u64::from(self.scalar_len) << 32)
        }
    }
}
