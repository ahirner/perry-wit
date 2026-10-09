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
pub struct Heap {
    end: Option<NonNull<u8>>,
    started: bool,
}

impl Heap {
    pub const fn new() -> Self {
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
