//! Checked byte ranges shared by the JSON and time helper adapters.

use core::{arch::wasm32, slice};

#[derive(Clone, Copy)]
pub(crate) struct GuestRange {
    pointer: usize,
    pub(crate) length: usize,
}

impl GuestRange {
    pub(crate) fn new(pointer: u32, length: u32) -> Result<Self, ()> {
        let end = u64::from(pointer) + u64::from(length);
        let memory_bytes = (wasm32::memory_size::<0>() as u64) * 65_536;
        if (pointer == 0 && length != 0)
            || length > isize::MAX as u32
            || end > memory_bytes
            || end > u64::from(u32::MAX)
        {
            return Err(());
        }
        Ok(Self {
            pointer: pointer as usize,
            length: length as usize,
        })
    }

    pub(crate) fn overlaps(self, other: Self) -> bool {
        self.length != 0
            && other.length != 0
            && self.pointer < other.pointer + other.length
            && other.pointer < self.pointer + self.length
    }

    /// The caller must guarantee initialized, immutable storage for the borrow.
    pub(crate) unsafe fn bytes<'a>(self) -> &'a [u8] {
        if self.length == 0 {
            return &[];
        }
        // SAFETY: construction checks nonnull, bounds, and isize limits; caller supplies ownership.
        unsafe { slice::from_raw_parts(self.pointer as *const u8, self.length) }
    }

    /// The caller must guarantee exclusive ownership for the returned borrow.
    pub(crate) unsafe fn bytes_mut<'a>(self) -> &'a mut [u8] {
        if self.length == 0 {
            return &mut [];
        }
        // SAFETY: construction checks nonnull, bounds, and isize limits; caller supplies exclusivity.
        unsafe { slice::from_raw_parts_mut(self.pointer as *mut u8, self.length) }
    }
}
