//! The `buffer` tier's memory source (design decision D3): [talc] over the memory after the
//! linked data and stack, then over pages it grows.
//!
//! `src/link` places every library's data and the shared stack at the start of memory and
//! resolves `__heap_base` to their end. The rest of the initial pages is claimed before any
//! page is grown, so a component runs within its initial memory when its working set fits
//! there (the stream contracts allow 64 KiB in total). Later growth extends the heap in place.
use core::{alloc::Layout, ptr::NonNull};
use talc::{base::Talc, base::binning::Binning, source::Source};

const PAGE: usize = 64 * 1024;

unsafe extern "C" {
    /// End of the linked data and stack (`src/link`).
    static __heap_base: u8;
}

/// Claims the initial memory's tail first, then grows and extends.
#[derive(Debug)]
pub(crate) struct Heap {
    end: Option<NonNull<u8>>,
    started: bool,
}

impl Heap {
    pub(crate) const fn new() -> Self {
        Self {
            end: None,
            started: false,
        }
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: `acquire` claims memory nothing else uses and calls no other allocator.
unsafe impl Source for Heap {
    fn acquire<B: Binning>(talc: &mut Talc<Self, B>, layout: Layout) -> Result<(), ()> {
        let memory_end = core::arch::wasm32::memory_size::<0>() * PAGE;
        if !talc.source.started {
            talc.source.started = true;
            // SAFETY: the linker resolves the symbol's address to the end of data and stack.
            let base = unsafe { &__heap_base as *const u8 as usize };
            if memory_end > base {
                // SAFETY: memory past the stack is unused until claimed here.
                talc.source.end = unsafe { talc.claim(base as *mut u8, memory_end - base) };
                if talc.source.end.is_some() {
                    return Ok(());
                }
            }
        }
        let required = layout.size() + layout.align() + 64;
        let pages = required.div_ceil(PAGE);
        let previous = core::arch::wasm32::memory_grow::<0>(pages);
        if previous == usize::MAX {
            return Err(());
        }
        let base = (previous * PAGE) as *mut u8;
        let size = pages * PAGE;
        if let Some(end) = talc.source.end.take()
            && end.as_ptr() == base
        {
            // SAFETY: the grown pages directly follow the heap.
            talc.source.end = Some(unsafe { talc.extend(end, base.wrapping_add(size)) });
            return Ok(());
        }
        // SAFETY: freshly grown pages.
        talc.source.end = unsafe { talc.claim(base, size) };
        talc.source.end.map(|_| ()).ok_or(())
    }
}

/// Single-threaded talc adapter exposing pressure information at compiler safe points.
pub(crate) struct Allocator(
    core::cell::UnsafeCell<talc::cell::TalcCell<Heap, talc::wasm::WasmBinning>>,
);
// SAFETY: components have one guest thread and never use shared-memory atomics.
unsafe impl Sync for Allocator {}
impl Default for Allocator {
    fn default() -> Self {
        Self::new()
    }
}
impl Allocator {
    pub(crate) const fn new() -> Self {
        const {
            assert!(!cfg!(target_feature = "atomics"));
        }
        Self(core::cell::UnsafeCell::new(talc::cell::TalcCell::new(
            Heap::new(),
        )))
    }
    #[cfg(feature = "collectible")]
    pub(crate) fn available(&self) -> usize {
        let counters = unsafe { &*self.0.get() }.counters();
        if counters.heap_count == 0 {
            core::arch::wasm32::memory_size::<0>() * PAGE - (&raw const __heap_base as usize)
        } else {
            counters.available_bytes
        }
    }
    /// Grow an exclusive allocation without moving it or collecting. Failure preserves its layout.
    ///
    /// # Safety
    /// `pointer` has the supplied allocation layout; `new_size` is a valid, larger layout size.
    #[cfg(feature = "collectible")]
    pub(crate) unsafe fn grow_in_place(
        &self,
        pointer: *mut u8,
        layout: Layout,
        new_size: usize,
    ) -> bool {
        // SAFETY: allocator operations are atomic; no other allocator reference is active.
        unsafe {
            let talc = (&mut *self.0.get()).get_mut();
            if talc.try_grow_in_place(pointer, layout, new_size) {
                return true;
            }
            let Some(end) = talc.source.end else {
                return false;
            };
            let Some(memory_end) = core::arch::wasm32::memory_size::<0>().checked_mul(PAGE) else {
                return false;
            };
            if end.as_ptr() as usize != memory_end {
                return false;
            }
            let reserved = talc.reserved(end);
            let payload_end = pointer as usize + layout.size();
            let Some(metadata) = (reserved.up_to.as_ptr() as usize).checked_sub(payload_end) else {
                return false;
            };
            // Talc rounds each payload plus its tag to CHUNK_UNIT. An intervening chunk
            // consumes at least another whole unit, so this bound identifies the final allocation.
            if !reserved.any || metadata == 0 || metadata > talc::base::CHUNK_UNIT {
                return false;
            }
            let Some(required) = (pointer as usize)
                .checked_add(new_size)
                .and_then(|end| end.checked_add(talc::base::CHUNK_UNIT))
            else {
                return false;
            };
            let pages = required.saturating_sub(memory_end).div_ceil(PAGE);
            if pages == 0 {
                return false;
            }
            let Some(new_end) = pages
                .checked_mul(PAGE)
                .and_then(|growth| memory_end.checked_add(growth))
            else {
                return false;
            };
            if core::arch::wasm32::memory_grow::<0>(pages) == usize::MAX {
                return false;
            }
            let new_end = new_end as *mut u8;
            talc.source.end = Some(talc.extend(end, new_end));
            talc.try_grow_in_place(pointer, layout, new_size)
        }
    }
}
// SAFETY: allocation/deallocation forward unchanged layouts and pointers to talc.
unsafe impl core::alloc::GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        unsafe { (&*self.0.get()).alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        unsafe { (&*self.0.get()).alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { (&*self.0.get()).dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        unsafe { (&*self.0.get()).realloc(pointer, layout, size) }
    }
}
