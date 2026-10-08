//! Allocator bake-off for perry's `buffer` tier and region chunk sources.
#![no_std]
use core::alloc::{GlobalAlloc, Layout};

#[cfg(any(feature = "talc", feature = "talc_noip"))]
#[global_allocator]
static A: talc::wasm::WasmDynamicTalc = talc::wasm::new_wasm_dynamic_allocator();
#[cfg(feature = "dlmalloc")]
#[global_allocator]
static A: dlmalloc::GlobalDlmalloc = dlmalloc::GlobalDlmalloc;
#[cfg(feature = "rlsf")]
#[global_allocator]
static A: rlsf::SmallGlobalTlsf = rlsf::SmallGlobalTlsf::new();
#[cfg(feature = "rlsf_full")]
#[global_allocator]
static A: rlsf::GlobalTlsf = rlsf::GlobalTlsf::new();
#[cfg(feature = "lol")]
#[global_allocator]
static A: lol_alloc::AssumeSingleThreaded<lol_alloc::FreeListAllocator> =
    unsafe { lol_alloc::AssumeSingleThreaded::new(lol_alloc::FreeListAllocator::new()) };

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

fn layout(size: usize) -> Layout {
    unsafe { Layout::from_size_align_unchecked(size, 8) }
}

/// The canonical ABI entry point: the only export of the size build.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn cabi_realloc(ptr: *mut u8, old: usize, align: usize, new: usize) -> *mut u8 {
    unsafe {
        let old_layout = Layout::from_size_align_unchecked(old, align);
        let p = if new == 0 {
            A.dealloc(ptr, old_layout);
            align as *mut u8
        } else if old == 0 {
            A.alloc(Layout::from_size_align_unchecked(new, align))
        } else {
            A.realloc(ptr, old_layout, new)
        };
        if p.is_null() {
            core::arch::wasm32::unreachable()
        }
        p
    }
}

#[cfg(feature = "workloads")]
mod workloads {
    use super::*;
    static mut SEED: u32 = 0x9e3779b9;
    fn rand() -> u32 {
        unsafe {
            let mut x = SEED;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            SEED = x;
            x
        }
    }
    unsafe fn touch(p: *mut u8, n: usize) {
        unsafe {
            p.write_volatile(1);
            p.add(n - 1).write_volatile(1);
        }
    }

    /// Stream reads: one chunk buffer per read, released at the transfer acknowledgement.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn stream_chunks(n: u32, size: u32) -> u32 {
        unsafe {
            for _ in 0..n {
                let p = A.alloc(layout(size as usize));
                touch(p, size as usize);
                A.dealloc(p, layout(size as usize));
            }
        }
        n
    }

    /// An unbounded body read: append `chunk` bytes per step up to `total`, like list lowering.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn grow_body(total: u32, chunk: u32) -> u32 {
        unsafe {
            let mut size = chunk as usize;
            let mut p = A.alloc(layout(size));
            let mut cap = size;
            while size < total as usize {
                size += chunk as usize;
                if size > cap {
                    let new = (cap * 2).max(size);
                    p = A.realloc(p, layout(cap), new);
                    cap = new;
                }
                touch(p, size);
            }
            A.dealloc(p, layout(cap));
        }
        total
    }

    /// The same body appended chunk by chunk with exact-size reallocs (worst case).
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn grow_exact(total: u32, chunk: u32) -> u32 {
        unsafe {
            let mut size = chunk as usize;
            let mut p = A.alloc(layout(size));
            while size < total as usize {
                p = A.realloc(p, layout(size), size + chunk as usize);
                size += chunk as usize;
                touch(p, size);
            }
            A.dealloc(p, layout(size));
        }
        total
    }

    const SLOTS: usize = 1024;
    static mut LIVE: [(usize, usize); SLOTS] = [(0, 0); SLOTS];
    fn churn_size() -> usize {
        let r = rand();
        match r % 100 {
            0..70 => 16 + (r >> 8) as usize % 240,
            70..95 => 256 + (r >> 8) as usize % 3840,
            _ => 4096 + (r >> 8) as usize % 61440,
        }
    }

    /// Owner-scoped canonical buffers: random sizes, random release order, ~1024 live.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn churn(steps: u32) -> u32 {
        unsafe {
            let live = &mut *(&raw mut LIVE);
            for _ in 0..steps {
                let slot = rand() as usize % SLOTS;
                let (p, n) = live[slot];
                if p != 0 {
                    A.dealloc(p as *mut u8, layout(n));
                }
                let n = churn_size();
                let p = A.alloc(layout(n));
                touch(p, n);
                live[slot] = (p as usize, n);
            }
            for slot in live.iter_mut() {
                if slot.0 != 0 {
                    A.dealloc(slot.0 as *mut u8, layout(slot.1));
                    *slot = (0, 0);
                }
            }
        }
        steps
    }

    /// Task regions: 64 KiB chunks acquired per task and released in bulk, interleaved
    /// with long-lived small buffers that pin fragments between tasks.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn regions(tasks: u32, chunks: u32) -> u32 {
        unsafe {
            let mut held = [0usize; 64];
            let live = &mut *(&raw mut LIVE);
            for t in 0..tasks {
                let count = 1 + rand() % chunks;
                for i in 0..count as usize {
                    let p = A.alloc(layout(65536));
                    touch(p, 65536);
                    held[i] = p as usize;
                    // A buffer outlives the task now and then (instance-owned bytes).
                    if rand() % 8 == 0 {
                        let slot = rand() as usize % SLOTS;
                        if live[slot].0 != 0 {
                            A.dealloc(live[slot].0 as *mut u8, layout(live[slot].1));
                        }
                        let n = churn_size();
                        live[slot] = (A.alloc(layout(n)) as usize, n);
                    }
                }
                for p in &held[..count as usize] {
                    A.dealloc(*p as *mut u8, layout(65536));
                }
                let _ = t;
            }
            for slot in live.iter_mut() {
                if slot.0 != 0 {
                    A.dealloc(slot.0 as *mut u8, layout(slot.1));
                    *slot = (0, 0);
                }
            }
        }
        tasks
    }
}
