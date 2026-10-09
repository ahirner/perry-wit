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
pub mod bytes;
#[cfg(target_arch = "wasm32")]
mod cli;
pub mod exception;
#[cfg(target_arch = "wasm32")]
mod filesystem;
mod import_buffer;
pub mod json;
#[cfg(target_arch = "wasm32")]
mod output;
pub mod process;
pub mod promise;
pub mod region;
pub mod string;
pub mod text;
pub mod text_bytes;

/// Reserved representation of an absent optional number.
pub const UNDEFINED_NUMBER: u64 = 0x7FF0_0000_0000_0001;

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
    use region::{
        chunk_counts, perry_alloc, perry_task_enter as perry_task_begin,
        perry_task_free as perry_task_end,
    };
    extern crate std;
    use std::string::String;

    /// The runtime state is a per-instance global, so the checks run in one test thread.
    #[test]
    fn runtime() {
        regions_release_in_bulk_and_reuse_chunks();
        arrays_grow_and_keep_elements();
        byte_views_preserve_aliases_and_validate_before_writing();
        json_trees_share_array_storage_and_retain_unicode();
        strings_append_in_place_and_share_prefixes();
        argument_lowering_opens_the_task_the_export_adopts();
        promises_queue_waiters_and_tasks_interleave();
        exceptions_are_owned_by_each_task_and_rejections_keep_identity();
        process_status_validates_before_replacing_instance_state();
    }

    fn json_trees_share_array_storage_and_retain_unicode() {
        use json::*;
        perry_task_begin();
        // SAFETY: literal strings have static lifetime and every allocated value stays in this task.
        unsafe {
            let input = r#"{"list":[1,2],"text":"é🙂","null":null}"#;
            let input = string::perry_string_adopt(input.as_ptr().cast_mut(), input.len() as u32);
            let root = perry_json_parse(input);
            assert_eq!(exception::perry_exception_status(), 0);
            let key = string::perry_string_adopt(b"list".as_ptr().cast_mut(), 4);
            let list = perry_json_get(root, key);
            let storage = perry_json_as_array(list);
            let value = perry_json_number(17.0);
            array::set_slot(storage, 0, value as usize as u64);
            assert_eq!(perry_json_get(root, key), list);
            assert_eq!(perry_json_as_number(perry_json_index(list, 0.0)), 17.0);
            assert_eq!(perry_json_equal(perry_json_array(storage), list), 1);
            let keys = array::from_refs([key, key].into_iter());
            let values = array::from_refs([list, list].into_iter());
            let wrapper = perry_json_object(keys, values);
            assert_eq!(
                string::text(perry_json_stringify(wrapper)),
                r#"{"list":[17,2]}"#
            );
            let output = perry_json_stringify(root);
            assert_eq!(
                string::text(output),
                r#"{"list":[17,2],"text":"é🙂","null":null}"#
            );
            assert!(perry_json_stringify(core::ptr::null_mut()).is_null());
            assert_eq!(exception::perry_exception_status(), 0);
            perry_json_as_number(list);
            assert_eq!(exception::perry_exception_status(), exception::TYPE_ERROR);
            exception::perry_exception_catch(exception::TYPE_ERROR as i32);
        }
        perry_task_end();
        assert_eq!(chunk_counts().0, 0);
    }

    fn process_status_validates_before_replacing_instance_state() {
        use process::*;
        perry_task_begin();
        perry_process_clear_exit_code();
        assert_eq!(perry_process_get_exit_code().to_bits(), UNDEFINED_NUMBER);
        assert_eq!(perry_process_exit_status(), -1);
        for (value, code) in [
            (0.0, 0),
            (-1.0, 255),
            (258.0, 2),
            (9_007_199_254_740_991.0, 255),
        ] {
            assert_eq!(perry_process_set_exit_code(value), 0);
            assert_eq!(perry_process_get_exit_code(), value);
            assert_eq!(perry_process_exit_status(), code);
        }
        for value in [
            1.5,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            9_007_199_254_740_992.0,
        ] {
            let status = perry_process_set_exit_code(value);
            assert_ne!(status, 0);
            // SAFETY: the checked setter raised a task-owned exception.
            unsafe {
                let error = exception::perry_exception_catch(status);
                assert_eq!(
                    exception::perry_exception_is(error, exception::RANGE_ERROR),
                    1
                );
            }
            assert_eq!(perry_process_get_exit_code(), 9_007_199_254_740_991.0);
        }
        perry_task_end();
        assert_eq!(perry_process_exit_status(), 255);
        perry_process_clear_exit_code();
        assert_eq!(perry_process_exit_status(), -1);
    }

    fn byte_views_preserve_aliases_and_validate_before_writing() {
        use bytes::*;
        perry_task_begin();
        let undefined = f64::from_bits(UNDEFINED_NUMBER);
        let values = [257.9, -1.9, f64::NAN, f64::INFINITY, -0.0, 511.0];
        let array = perry_bytes_new(values.len() as f64);
        // SAFETY: all views are live in this task; indices deliberately exercise checked APIs.
        unsafe {
            for (index, value) in values.into_iter().enumerate() {
                assert_eq!(perry_bytes_get(array, index as f64), 0.0);
                perry_bytes_put(array, index as f64, value);
            }
            for (index, expected) in [1.0, 255.0, 0.0, 0.0, 0.0, 255.0].into_iter().enumerate() {
                assert_eq!(perry_bytes_get(array, index as f64), expected);
            }
            for index in [-1.0, 0.5, 6.0, f64::NAN, f64::INFINITY] {
                assert_eq!(perry_bytes_get(array, index).to_bits(), UNDEFINED_NUMBER);
                perry_bytes_put(array, index, 42.0);
            }
            let alias = perry_bytes_subarray(array, 1.0, undefined);
            let copy = perry_bytes_slice(array, 1.0, undefined);
            assert_eq!(perry_bytes_buffer(alias), perry_bytes_buffer(array));
            assert_ne!(perry_bytes_buffer(copy), perry_bytes_buffer(array));
            assert_eq!(perry_bytes_offset(alias), 1.0);
            assert_eq!(perry_bytes_length(alias), 5.0);
            assert_eq!(perry_bytes_set(array, alias, 0.0), 0);
            assert_eq!(perry_bytes_get(array, 0.0), 255.0);
            assert_eq!(perry_bytes_get(alias, 0.0), 0.0);
            assert_eq!(perry_bytes_get(copy, 0.0), 255.0);
            let before: Vec<_> = (0..6).map(|i| perry_bytes_get(array, i as f64)).collect();
            for offset in [-1.0, 2.0, f64::INFINITY] {
                let status = perry_bytes_set(array, alias, offset);
                let error = exception::perry_exception_catch(status);
                assert_eq!(
                    exception::perry_exception_is(error, exception::RANGE_ERROR),
                    1
                );
                assert_eq!(
                    (0..6)
                        .map(|i| perry_bytes_get(array, i as f64))
                        .collect::<Vec<_>>(),
                    before
                );
            }
            let empty = perry_bytes_subarray(array, -2.0, f64::NAN);
            assert_eq!(perry_bytes_length(empty), 0.0);
            assert_eq!(perry_bytes_offset(empty), 4.0);
            for length in [-1.0, f64::INFINITY, u32::MAX as f64 + 1.0] {
                assert!(perry_bytes_new(length).is_null());
                let error = exception::perry_exception_catch(exception::RANGE_ERROR as i32);
                assert_eq!(
                    exception::perry_exception_is(error, exception::RANGE_ERROR),
                    1
                );
            }
        }
        perry_task_end();
        assert_eq!(chunk_counts().0, 0, "byte allocations outlived their task");
    }

    fn exceptions_are_owned_by_each_task_and_rejections_keep_identity() {
        use exception::*;
        use promise::*;
        use region::*;
        let a = perry_task_enter();
        let original = perry_exception_new(NUMBER, 42.0f64.to_bits());
        let promise = perry_promise_new();
        // SAFETY: all references belong to the currently running task.
        unsafe {
            let status = perry_exception_raise(original);
            perry_promise_reject(promise, status);
            assert!(current().exception.is_null());
            let status = perry_promise_rethrow(promise);
            assert_eq!(perry_exception_catch(status), original);
            perry_exception_raise(original);
            perry_task_suspend();
            perry_task_enter();
            assert!(current().exception.is_null());
            let other = perry_exception_catch(TYPE_ERROR as i32);
            assert_eq!(perry_exception_is(other, ERROR), 1);
            assert_eq!(perry_exception_is(other, RANGE_ERROR), 0);
            perry_task_free();
            perry_task_resume(a);
            assert_eq!(perry_exception_catch(NUMBER as i32), original);
            let status = perry_promise_rethrow(promise);
            assert_eq!(perry_exception_catch(status), original);
            assert_eq!(perry_exception_is(original, ERROR), 0);
        }
        perry_task_free();
    }

    fn promises_queue_waiters_and_tasks_interleave() {
        use promise::*;
        use region::*;
        let a = perry_task_enter();
        let p = perry_promise_new();
        let (f1, f2) = (perry_alloc(16), perry_alloc(16));
        perry_promise_wait(p, f1);
        perry_promise_wait(p, f2);
        assert!(perry_ready_pop().is_null());
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
        assert_eq!(perry_ready_pop(), f1);
        assert_eq!(perry_ready_pop(), f2);
        assert!(perry_ready_pop().is_null());
        assert_eq!(perry_promise_wait(p, f2), 0);
        assert!(perry_ready_pop().is_null());
        current().push_ready(f1);
        assert_eq!(perry_promise_wait(p, f2), 1);
        assert_eq!(perry_ready_pop(), f1);
        assert_eq!(perry_ready_pop(), f2);
        assert!(perry_ready_pop().is_null());
        // Settling twice is a no-op; rejection records the status.
        let q = perry_promise_new();
        perry_promise_reject(q, 2);
        perry_promise_resolve(q, 1);
        assert_eq!(
            unsafe { (*(q as *const u32), *(q.add(4) as *const i32)) },
            (REJECTED, 2)
        );
        unsafe {
            perry_promise_observe(q);
        }
        assert_eq!(perry_task_failed(), 0);
        assert!(
            current().rejections.is_null(),
            "handled rejections were retained for rescanning"
        );
        let unhandled = perry_promise_new();
        perry_promise_reject(unhandled, 2);
        assert_eq!(perry_task_failed(), 1);
        perry_task_free();
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
