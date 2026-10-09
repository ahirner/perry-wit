//! Runtime for components built by the MIR backend (`TODOs.md` P5).
//!
//! Built like the helper libraries (position-independent, imported memory) and linked into
//! every compiled module by `src/link`, which resolves the `__perry_helper` imports of
//! compiled code against the `perry_*` exports here. Memory tiers (design decision D3):
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
pub mod string;

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
    extern crate std;
    use std::string::String;

    /// The runtime state is a per-instance global, so the checks run in one test thread.
    #[test]
    fn runtime() {
        regions_release_in_bulk_and_reuse_chunks();
        arrays_grow_and_keep_elements();
        strings_append_in_place_and_share_prefixes();
        argument_lowering_opens_the_task_the_export_adopts();
    }

    fn text(string: *mut u8) -> &'static str {
        unsafe {
            let len = *(string as *const u32);
            let data = *(string.add(8) as *const *const u8);
            core::str::from_utf8(core::slice::from_raw_parts(data, len as usize)).unwrap()
        }
    }

    fn strings_append_in_place_and_share_prefixes() {
        perry_task_begin();
        let mut x = String::from("x");
        let x = unsafe { string::perry_string_adopt(x.as_mut_ptr(), 1) };
        let mut emoji = String::from("é🙂");
        let emoji = unsafe { string::perry_string_adopt(emoji.as_mut_ptr(), 6) };
        assert_eq!(unsafe { *(emoji.add(4) as *const u32) }, 3);
        let mut text_value = unsafe { string::perry_string_concat(x, x) };
        let first = text_value;
        for _ in 0..1000 {
            text_value = unsafe { string::perry_string_concat(text_value, x) };
        }
        assert_eq!(text(text_value).len(), 1002);
        assert_eq!(text(first), "xx");
        // Appending to an older string must not clobber the newer one sharing its buffer.
        let branch = unsafe { string::perry_string_concat(first, emoji) };
        assert_eq!(text(branch), "xxé🙂");
        assert_eq!(&text(text_value)[..3], "xxx");
        assert_eq!(unsafe { *(branch.add(4) as *const u32) }, 5);
        let chunks_before = chunk_counts().0;
        for _ in 0..100_000 {
            text_value = unsafe { string::perry_string_concat(text_value, x) };
        }
        // Linear: about 16 bytes of header per append plus amortized doubling.
        assert!(
            chunk_counts().0 - chunks_before < 64,
            "{:?}",
            chunk_counts()
        );
        perry_task_end();
    }

    fn argument_lowering_opens_the_task_the_export_adopts() {
        let before = chunk_counts();
        let block = unsafe { region::perry_cabi_realloc(core::ptr::null_mut(), 0, 1, 40_000) };
        assert!(!block.is_null());
        region::perry_task_enter();
        perry_alloc(64);
        perry_task_end();
        let after = chunk_counts();
        assert_eq!((after.0, after.1), (before.0, before.1));
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
