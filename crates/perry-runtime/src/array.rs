//! Dense arrays with 8-byte element slots.
//!
//! Layout, read and written inline by compiled code: `len: u32` at 0, `cap: u32` at 4,
//! `data: u32` at 8. Elements are `f64` or a 32-bit reference in the low half of a slot.
//! `cap` is never 0, so compiled code may read slot 0 unconditionally (branch-free bounds
//! checks select the result afterwards). Growth doubles capacity and preserves live elements.
//! Collectible arrays release old storage through tracing; proven bounded task allocations
//! retain their storage until task completion.
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

const ARRAY_SHAPE: [u32; 3] = [0, 1, core::mem::offset_of!(Array, data) as u32];
const REFERENCE_SLOTS: [u32; 3] = [SLOT, 1, 0];

/// An array whose header and element storage are collectible. Reference slots are described
/// precisely; numeric slots are opaque, even when their bits resemble guest addresses.
#[unsafe(no_mangle)]
pub extern "C" fn perry_array_new_collectible(capacity: u32, references: u32) -> *mut u8 {
    let cap = capacity.max(MIN_CAPACITY);
    let shape = if references != 0 {
        REFERENCE_SLOTS.as_ptr()
    } else {
        core::ptr::null()
    };
    // SAFETY: both static shapes describe these zeroed allocations exactly.
    unsafe {
        let array = crate::roots::perry_gc_alloc(
            core::mem::size_of::<Array>() as u32,
            ARRAY_SHAPE.as_ptr(),
        ) as *mut Array;
        let data = crate::roots::perry_gc_alloc(
            cap.checked_mul(SLOT).expect("array capacity overflow"),
            shape,
        ) as *mut u64;
        array.write(Array { len: 0, cap, data });
        array as *mut u8
    }
}

/// Grow collectible element storage. Allocation does not collect inside this helper.
///
/// # Safety
/// `array` was created by [perry_array_new_collectible], with the same reference flag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_grow_collectible(array: *mut u8, references: u32) {
    let array = array as *mut Array;
    let shape = if references != 0 {
        REFERENCE_SLOTS.as_ptr()
    } else {
        core::ptr::null()
    };
    // SAFETY: the caller owns a live array, and the copied slots retain their original shape.
    unsafe {
        let cap = (*array)
            .cap
            .checked_mul(2)
            .expect("array capacity overflow");
        let data = crate::roots::resize_buffer(
            (*array).data.cast(),
            (*array)
                .cap
                .checked_mul(SLOT)
                .expect("array capacity overflow"),
            cap.checked_mul(SLOT).expect("array capacity overflow"),
            (*array)
                .len
                .checked_mul(SLOT)
                .expect("array length overflow"),
            shape,
        ) as *mut u64;
        (*array).data = data;
        (*array).cap = cap;
    }
}

/// Helper arrays have statically known element representation.
#[unsafe(no_mangle)]
pub extern "C" fn perry_array_new_typed(capacity: u32, references: u32) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        perry_array_new_collectible(capacity, references)
    }
    #[cfg(not(feature = "collectible"))]
    {
        let _ = references;
        perry_array_new(capacity)
    }
}
