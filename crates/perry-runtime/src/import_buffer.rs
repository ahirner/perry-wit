//! Canonical import allocations have no task context while the host lowers a result.
//! Consumers either release these buffers or transfer them to a live task explicitly.
use crate::region::Task;
#[cfg(target_arch = "wasm32")]
use crate::region::current;
use alloc::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use core::ptr;

const ALIGN: usize = 8;
#[repr(C)]
pub(crate) struct Block {
    next: *mut Block,
    owner: *mut Task,
    size: usize,
}
const HEADER: usize = size_of::<Block>().next_multiple_of(ALIGN);

fn layout(size: usize) -> Layout {
    Layout::from_size_align(
        HEADER
            .checked_add(size)
            .expect("import allocation too large"),
        ALIGN,
    )
    .expect("import allocation layout")
}
unsafe fn header(data: *mut u8) -> *mut Block {
    // SAFETY: every nonempty imported allocation has this prefix.
    unsafe { data.sub(HEADER).cast() }
}

/// # Safety
/// `old` is null, the empty sentinel, or an unclaimed allocation of `old_size` bytes
/// from this allocator. Canonical ABI alignment is at most eight.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_import_realloc(
    old: *mut u8,
    old_size: u32,
    align: u32,
    new_size: u32,
) -> *mut u8 {
    assert!(align <= ALIGN as u32);
    // SAFETY: an unclaimed allocation has no other owner; reallocation copies only its live prefix.
    unsafe {
        let result = if new_size == 0 {
            ALIGN as *mut u8
        } else {
            let layout = layout(new_size as usize);
            let block = alloc(layout).cast::<Block>();
            if block.is_null() {
                handle_alloc_error(layout);
            }
            block.write(Block {
                next: ptr::null_mut(),
                owner: ptr::null_mut(),
                size: new_size as usize,
            });
            block.cast::<u8>().add(HEADER)
        };
        if old.addr() > ALIGN {
            assert!(
                (*header(old)).owner.is_null(),
                "host reallocated an owned import result"
            );
            ptr::copy_nonoverlapping(old, result, old_size.min(new_size) as usize);
            release(old);
        }
        result
    }
}

/// # Safety
/// `data` is empty or an unclaimed imported allocation. It is not used afterward.
pub(crate) unsafe fn release(data: *mut u8) {
    if data.addr() <= ALIGN {
        return;
    }
    // SAFETY: the unique unclaimed block owns its allocation.
    unsafe {
        let block = header(data);
        assert!(
            (*block).owner.is_null(),
            "import result already belongs to a task"
        );
        dealloc(block.cast(), layout((*block).size));
    }
}

/// # Safety
/// `data` is empty or an imported allocation, transferred to the current live task.
#[cfg(target_arch = "wasm32")]
pub(crate) unsafe fn claim(data: *mut u8) {
    if data.addr() <= ALIGN {
        return;
    }
    let task = current() as *mut Task;
    // SAFETY: ownership changes once; a repeated claim by the same task is harmless.
    unsafe {
        let block = header(data);
        if (*block).owner == task {
            return;
        }
        assert!(
            (*block).owner.is_null(),
            "import result belongs to another task"
        );
        (*block).owner = task;
        (*block).next = (*task).imported;
        (*task).imported = block;
    }
}

/// # Safety
/// The task has finished all native work and no returned value remains borrowed by the host.
pub(crate) unsafe fn release_task(task: *mut Task) {
    // SAFETY: each owned buffer is linked exactly once and no longer has a live consumer.
    unsafe {
        let mut block = (*task).imported;
        while !block.is_null() {
            let next = (*block).next;
            dealloc(block.cast(), layout((*block).size));
            block = next;
        }
        (*task).imported = ptr::null_mut();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unclaimed_buffers_preserve_bytes_across_growth_shrink_and_empty_release() {
        // SAFETY: each reallocation consumes the previous buffer; reads stay in initialized storage.
        unsafe {
            let data = perry_import_realloc(ptr::null_mut(), 0, 8, 128);
            assert_eq!(data.addr() % 8, 0);
            for index in 0..128 {
                data.add(index).write(index as u8);
            }
            let data = perry_import_realloc(data, 128, 8, 4096);
            assert_eq!(data.addr() % 8, 0);
            for index in 0..128 {
                assert_eq!(*data.add(index), index as u8);
            }
            let data = perry_import_realloc(data, 4096, 8, 9);
            assert_eq!(
                core::slice::from_raw_parts(data, 9),
                &[0, 1, 2, 3, 4, 5, 6, 7, 8]
            );
            let empty = perry_import_realloc(data, 9, 8, 0);
            release(empty);
            release(ptr::null_mut());
        }
    }
}
