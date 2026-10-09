//! Dense arrays (subset rule R8) with 8-byte element slots.
//!
//! Layout, read and written inline by compiled code: `len: u32` at 0, `cap: u32` at 4,
//! `data: u32` at 8. Elements are `f64` or a 32-bit reference in the low half of a slot.
//! `cap` is never 0, so compiled code may read slot 0 unconditionally (branch-free bounds
//! checks select the result afterwards). Storage lives in the task region: growth copies and
//! leaves the old block to the region, which bounds waste by the doubling.
use crate::region::perry_alloc;

pub const LEN: u32 = 0;
pub const CAP: u32 = 4;
pub const DATA: u32 = 8;
pub const HEADER: u32 = 12;
pub const SLOT: u32 = 8;
const MIN_CAPACITY: u32 = 4;

#[cfg(target_arch = "wasm32")]
const _: () = assert!(core::mem::size_of::<Array>() == HEADER as usize);

#[repr(C)]
struct Array {
    len: u32,
    cap: u32,
    data: *mut u64,
}

/// A new array with room for at least `capacity` elements and length 0.
#[unsafe(no_mangle)]
pub extern "C" fn perry_array_new(capacity: u32) -> *mut u8 {
    let cap = capacity.max(MIN_CAPACITY);
    let array = perry_alloc(core::mem::size_of::<Array>() as u32) as *mut Array;
    let data = perry_alloc(cap * SLOT) as *mut u64;
    // SAFETY: both blocks were just allocated with sufficient size and alignment.
    unsafe { array.write(Array { len: 0, cap, data }) };
    array as *mut u8
}

/// Double the capacity of `array`; called by compiled code when a push finds it full.
///
/// # Safety
/// `array` must come from [perry_array_new].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_grow(array: *mut u8) {
    let array = array as *mut Array;
    // SAFETY: the caller passes a live array; the new block holds every old element.
    unsafe {
        let cap = (*array)
            .cap
            .checked_mul(2)
            .expect("array capacity overflow");
        let data = perry_alloc(cap * SLOT) as *mut u64;
        core::ptr::copy_nonoverlapping((*array).data, data, (*array).len as usize);
        (*array).data = data;
        (*array).cap = cap;
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn from_refs(values: impl ExactSizeIterator<Item = *mut u8>) -> *mut u8 {
    let len = values.len() as u32;
    let array = perry_array_new(len) as *mut Array;
    // SAFETY: the newly allocated array has room for each reference.
    unsafe {
        for (index, value) in values.enumerate() {
            (*array).data.add(index).write(value as u64);
        }
        (*array).len = len;
    }
    array.cast()
}
