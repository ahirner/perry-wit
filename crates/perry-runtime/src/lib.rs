//! Runtime for components built by the MIR backend (`TODOs.md` P5).
//!
//! The module owns linear memory and exports it as `memory`; compiled code imports it together
//! with the functions below under the module name [MODULE]. Memory tiers (design decision D3):
//!
//! - `buffer`: [talc] (`WasmDynamicTalc`), chosen by the bake-off in `bench/allocators`:
//!   1.9 KB of code against dlmalloc's 6.7 KB, in-place `realloc`, equal or better speed.
//! - `task`: [region] bump allocation in 64 KiB chunks taken from the buffer allocator and
//!   released in bulk at task end.
//! - `instance`: the collector (P5.3) is not built yet. The MIR builder cannot express module
//!   state (R4) yet, so no allocation can outlive its task and every site is `task`.
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

pub mod array;
pub mod region;

/// Import module name for the functions and memory of this runtime.
pub const MODULE: &str = "perry_rt";

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: talc::wasm::WasmDynamicTalc = talc::wasm::new_wasm_dynamic_allocator();

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// ECMAScript `%` on numbers: the IEEE remainder truncated toward zero (C `fmod`).
#[unsafe(no_mangle)]
pub extern "C" fn perry_f64_rem(dividend: f64, divisor: f64) -> f64 {
    libm::fmod(dividend, divisor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use region::{chunk_counts, perry_alloc, perry_task_begin, perry_task_end};

    /// The runtime state is a per-instance global, so the checks run in one test thread.
    #[test]
    fn runtime() {
        regions_release_in_bulk_and_reuse_chunks();
        arrays_grow_and_keep_elements();
    }

    fn regions_release_in_bulk_and_reuse_chunks() {
        perry_task_begin();
        let first = perry_alloc(24) as usize;
        let second = perry_alloc(8) as usize;
        assert_eq!(first % 8, 0);
        assert_eq!(second, first + 24);
        for _ in 0..5000 {
            perry_alloc(100);
        }
        let large = perry_alloc(1 << 20);
        unsafe { large.add((1 << 20) - 1).write(1) };
        let (chunks, dedicated, _) = chunk_counts();
        assert!(chunks >= 8, "{chunks}");
        assert_eq!(dedicated, 1);
        perry_task_end();
        let (chunks_after, dedicated_after, free) = chunk_counts();
        assert_eq!((chunks_after, dedicated_after), (0, 0));
        assert_eq!(free, chunks);
        // The next task reuses the free chunks instead of allocating.
        perry_task_begin();
        for _ in 0..5000 {
            perry_alloc(100);
        }
        assert_eq!(chunk_counts().2, 0);
        perry_task_end();
    }

    fn arrays_grow_and_keep_elements() {
        perry_task_begin();
        let array = array::perry_array_new(0);
        unsafe {
            let header = array as *mut u32;
            assert_eq!((*header, *header.add(1)), (0, 4));
            for i in 0..100u64 {
                if *header == *header.add(1) {
                    array::perry_array_grow(array);
                }
                let data = *(array.add(8) as *const *mut u64);
                data.add(*header as usize).write(i);
                *header += 1;
            }
            let data = *(array.add(8) as *const *mut u64);
            assert_eq!(*data.add(99), 99);
            assert_eq!(*header.add(1), 128);
        }
        perry_task_end();
        assert_eq!(perry_f64_rem(7.5, 2.0), 1.5);
        assert_eq!(perry_f64_rem(-7.5, 2.0), -1.5);
    }
}
