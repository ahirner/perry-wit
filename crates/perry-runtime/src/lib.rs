//! Runtime services linked on demand into compiled components.
//!
//! `talc` supplies linear-memory storage. Proven bounded tasks use regions; unproven and
//! persistent allocations use precise non-moving collection. Compiled safe points root live
//! values, while suspended operations retain explicit owners through acknowledgement.
//! Promise observers reside in their frames; proven stream leases reuse their storage.
#![cfg_attr(target_arch = "wasm32", no_std)]

extern crate alloc;

mod abort;
pub mod array;
#[cfg(feature = "collectible")]
mod buffers;
mod collectible;
pub mod exception;
pub mod foreign;
pub mod host;
mod instance;
#[cfg(not(target_arch = "wasm32"))]
pub use instance::native::RuntimeInstance;
pub mod io;
pub mod promise;
pub mod region;
#[cfg(any(feature = "collectible", not(target_arch = "wasm32")))]
mod resources;
pub mod roots;
pub mod string;

#[cfg(target_arch = "wasm32")]
mod heap;

#[cfg(target_arch = "wasm32")]
#[global_allocator]
static ALLOCATOR: heap::Allocator = heap::Allocator::new();

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// Bits of `undefined` in a `number | undefined` slot (`perry_abi::UNDEFINED_BITS`), the
/// value of `Promise<void>`.
pub(crate) const UNDEFINED_BITS: u64 = 0x7FF0_0000_0000_0001;

/// ECMAScript `%` on numbers: the IEEE remainder truncated toward zero (C `fmod`).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_f64_rem(dividend: f64, divisor: f64) -> f64 {
    libm::fmod(dividend, divisor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use region::{
        chunk_counts, perry_alloc, perry_task_enter as perry_task_begin,
        perry_task_free as perry_task_end,
    };
    extern crate std;
    use std::string::String;

    #[test]
    fn output_drains_through_blocked_writes_and_closes() {
        crate::RuntimeInstance::default().enter(|| {
            use host::mock::{complete_pending, with};
            use io::*;
            region::perry_task_enter();
            let mut hello = String::from("hello world");
            let hello = unsafe { string::perry_string_adopt(hello.as_mut_ptr(), 11) };
            let mut bang = String::from("!");
            let bang = unsafe { string::perry_string_adopt(bang.as_mut_ptr(), 1) };
            unsafe { perry_print(0, hello, 1) };
            perry_io_pump();
            // Output produced while a write is blocked waits in the second buffer.
            unsafe { perry_print(0, bang, 1) };
            unsafe { perry_print(1, bang, 0) };
            let mut rounds = 0;
            while perry_io_close() == 0 {
                let (handle, code) = complete_pending().expect("a blocked write");
                assert_eq!(perry_io_event(host::EVENT_STREAM_WRITE, handle, code), 1);
                rounds += 1;
                assert!(rounds < 20);
            }
            with(|host| {
                assert_eq!(&host.written[0][..], b"hello world\n!\n");
                assert_eq!(&host.written[1][..], b"!");
            });
            // A closed task drops further output instead of reopening its streams.
            unsafe { perry_print(0, bang, 1) };
            assert_eq!(perry_io_close(), 1);
            region::perry_task_free();
        });
    }

    #[test]
    fn promise_events_resume_observers_and_tasks_interleave() {
        crate::RuntimeInstance::default().enter(|| {
            use promise::*;
            use region::*;
            let a = perry_task_enter();
            let p = perry_promise_new();
            let (f1, f2) = (perry_alloc(80), perry_alloc(80));
            perry_promise_wait(p, f1, unsafe { f1.add(16) });
            perry_promise_wait(p, f2, unsafe { f2.add(16) });
            assert!(host::mock::completed_reactions().is_empty());
            perry_subtask_register(7, p, 42);
            perry_task_set_set(3);
            assert_eq!(perry_task_suspend(), 3);
            // Another task runs to completion while `a` waits on the host.
            let b = perry_task_enter();
            assert_ne!(a, b);
            perry_alloc(100_000);
            perry_task_free();
            unsafe { perry_task_resume(a) };
            assert_eq!(perry_subtask_done(9), 0);
            assert_eq!(perry_subtask_done(7), 1);
            assert_eq!(unsafe { *(p as *const u32) }, FULFILLED);
            assert_eq!(unsafe { *(p.add(VALUE as usize) as *const u64) }, 42);
            let events = host::mock::completed_reactions();
            assert_eq!(events.len(), 2);
            assert_eq!(perry_promise_event(host::EVENT_FUTURE_READ, events[0]), f1);
            assert_eq!(perry_promise_event(host::EVENT_FUTURE_READ, events[1]), f2);
            assert!(host::mock::completed_reactions().is_empty());
            // Settling twice is a no-op; rejection records the status.
            let q = perry_promise_new();
            perry_promise_reject(q, 2);
            perry_promise_resolve(q, 1);
            assert_eq!(
                unsafe { (*(q as *const u32), *(q.add(4) as *const i32)) },
                (REJECTED, 2)
            );
            perry_task_free();
        });
    }

    fn text(string: *mut u8) -> &'static str {
        unsafe {
            let len = *(string as *const u32);
            let data = *(string.add(8) as *const *const u8);
            core::str::from_utf8(core::slice::from_raw_parts(data, len as usize)).unwrap()
        }
    }

    #[test]
    fn strings_append_in_place_and_share_prefixes() {
        crate::RuntimeInstance::default().enter(|| {
            perry_task_begin();
            let mut x = String::from("x");
            let x = unsafe { string::perry_string_adopt(x.as_mut_ptr(), 1) };
            let mut emoji = String::from("é🙂");
            let emoji = unsafe { string::perry_string_adopt(emoji.as_mut_ptr(), 6) };
            assert_eq!(unsafe { *(emoji.add(4) as *const u32) }, 2);
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
            assert_eq!(unsafe { *(branch.add(4) as *const u32) }, 4);
            let chunks_before = chunk_counts().0;
            for _ in 0..100_000 {
                text_value = unsafe { string::perry_string_concat(text_value, x) };
            }
            // Linear header storage; buffer growth uses separate large chunks.
            assert!(
                chunk_counts().0 - chunks_before
                    <= (100_000 * core::mem::size_of::<string::Str>()).div_ceil(region::CHUNK - 64)
                        + 1,
                "{:?}",
                chunk_counts()
            );
            perry_task_end();
        });
    }

    #[test]
    fn argument_lowering_opens_the_task_the_export_adopts() {
        crate::RuntimeInstance::default().enter(|| {
            let before = chunk_counts();
            let block = unsafe { region::perry_cabi_realloc(core::ptr::null_mut(), 0, 1, 40_000) };
            assert!(!block.is_null());
            region::perry_task_enter();
            unsafe { region::perry_cabi_release(block) };
            perry_alloc(64);
            perry_task_end();
            let after = chunk_counts();
            assert_eq!((after.0, after.1), (before.0, before.1));
        });
    }

    #[cfg(not(feature = "collectible"))]
    #[test]
    fn regions_release_in_bulk_and_reuse_chunks() {
        crate::RuntimeInstance::default().enter(|| {
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
            let (chunks, dedicated, unused) = chunk_counts();
            assert!(chunks >= 8, "{chunks}");
            assert_eq!(dedicated, 1);
            perry_task_end();
            let (chunks_after, dedicated_after, free) = chunk_counts();
            assert_eq!((chunks_after, dedicated_after), (0, 0));
            assert_eq!(free, unused + chunks);
            // The next task reuses the free chunks instead of allocating.
            perry_task_begin();
            for _ in 0..5000 {
                perry_alloc(100);
            }
            assert_eq!(chunk_counts(), (chunks, 0, unused));
            perry_task_end();
        });
    }

    #[test]
    fn arrays_grow_and_keep_elements() {
        crate::RuntimeInstance::default().enter(|| {
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
        });
    }
}

#[path = "../../perry-abi/src/layout.rs"]
pub mod layout;
