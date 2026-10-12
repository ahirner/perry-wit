//! Dense arrays with 8-byte element slots.
//!
//! Layout, read and written inline by compiled code: `len: u32` at 0, `cap: u32` at 4,
//! `data: u32` at 8. Elements are `f64` or a 32-bit reference in the low half of a slot.
//! `cap` is never 0, so compiled code may read slot 0 unconditionally (branch-free bounds
//! checks select the result afterwards). Growth doubles capacity and preserves live elements.
//! Collectible arrays release old storage through tracing; proven bounded task allocations
//! retain their storage until task completion.
use crate::region::perry_alloc;

pub use crate::layout::array::*;
const MIN_CAPACITY: u32 = 4;

#[cfg(target_arch = "wasm32")]
const _: () = {
    assert!(core::mem::size_of::<Array>() == 12);
    assert!(core::mem::offset_of!(Array, len) == LEN as usize);
    assert!(core::mem::offset_of!(Array, cap) == CAP as usize);
    assert!(core::mem::offset_of!(Array, data) == DATA as usize);
};

#[repr(C)]
struct Array {
    len: u32,
    cap: u32,
    data: *mut u64,
}

impl Array {
    /// An empty array over `cap` slots at `data`, with length zero.
    fn empty(cap: u32, data: *mut u64) -> Self {
        Self { len: 0, cap, data }
    }
}

/// A new array with room for at least `capacity` elements and length 0.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_array_new(capacity: u32) -> *mut u8 {
    let cap = capacity.max(MIN_CAPACITY);
    let array = perry_alloc(core::mem::size_of::<Array>() as u32) as *mut Array;
    let data = perry_alloc(slot_bytes(cap)) as *mut u64;
    // SAFETY: both blocks were just allocated with sufficient size and alignment.
    unsafe { array.write(Array::empty(cap, data)) };
    array as *mut u8
}

/// Double the capacity of `array`; called by compiled code when a push finds it full.
///
/// # Safety
/// `array` must come from [perry_array_new].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_grow(array: *mut u8) {
    let array = array as *mut Array;
    // SAFETY: the caller passes a live array; the new block holds every old element.
    unsafe {
        let (cap, bytes) = doubled((*array).cap);
        let data = perry_alloc(bytes) as *mut u64;
        core::ptr::copy_nonoverlapping((*array).data, data, (*array).len as usize);
        (*array).data = data;
        (*array).cap = cap;
    }
}

/// Twice `cap`, with the byte size of that many slots; traps when either overflows.
fn doubled(cap: u32) -> (u32, u32) {
    let cap = cap.checked_mul(2).expect("array capacity overflow");
    (cap, slot_bytes(cap))
}

/// Byte size of `cap` slots; traps when it exceeds the 32-bit address space.
fn slot_bytes(cap: u32) -> u32 {
    crate::sequence::byte_size(cap, SLOT, 0)
}

const ARRAY_SHAPE: [u32; 3] = [0, 1, core::mem::offset_of!(Array, data) as u32];
const REFERENCE_SLOTS: [u32; 3] = [SLOT, 1, 0];

/// The element shape of a collectible array: precise reference slots or opaque numbers.
fn element_shape(references: u32) -> *const u32 {
    if references != 0 {
        REFERENCE_SLOTS.as_ptr()
    } else {
        core::ptr::null()
    }
}

/// An array whose header and element storage are collectible. Reference slots are described
/// precisely; numeric slots are opaque, even when their bits resemble guest addresses.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_array_new_collectible(capacity: u32, references: u32) -> *mut u8 {
    let cap = capacity.max(MIN_CAPACITY);
    // SAFETY: both static shapes describe these zeroed allocations exactly.
    unsafe {
        let array = crate::roots::perry_gc_alloc(
            core::mem::size_of::<Array>() as u32,
            ARRAY_SHAPE.as_ptr(),
        ) as *mut Array;
        let data =
            crate::roots::perry_gc_alloc(slot_bytes(cap), element_shape(references)) as *mut u64;
        array.write(Array::empty(cap, data));
        array as *mut u8
    }
}

/// Grow collectible element storage. Allocation does not collect inside this helper.
///
/// # Safety
/// `array` was created by [perry_array_new_collectible], with the same reference flag.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_grow_collectible(array: *mut u8, references: u32) {
    let array = array as *mut Array;
    // SAFETY: the caller owns a live array, and the copied slots retain their original shape.
    unsafe {
        let (cap, bytes) = doubled((*array).cap);
        // The old capacity and length are at most half the new one, so their sizes fit.
        let data = crate::roots::resize_buffer(
            (*array).data.cast(),
            (*array).cap * SLOT,
            bytes,
            (*array).len * SLOT,
            element_shape(references),
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

/// Concatenate arrays, consuming only headers and buffers proved exclusive by MIR.
///
/// # Safety
/// Both arrays are live, have the same element representation, and every operand selected
/// by `reuse` is exclusively owned and unobserved after this call. Allocation never collects.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_concat(
    left: *mut u8,
    right: *mut u8,
    references: u32,
    reuse: u32,
) -> *mut u8 {
    let candidates = if reuse == 2 { [1, 0] } else { [0, 1] };
    let count = reuse.count_ones();
    unsafe {
        perry_array_concat_many(
            [left, right].as_ptr(),
            2,
            references,
            candidates.as_ptr(),
            count,
        )
    }
}

/// Combine all segments with one reservation. Sources remain stable throughout this atomic helper.
///
/// # Safety
/// `sources` describes `count` live arrays with identical element representation. Selected
/// headers and storage are exclusive, and their old identities cannot be observed afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_concat_many(
    sources: *const *mut u8,
    count: u32,
    references: u32,
    candidates: *const u32,
    candidate_count: u32,
) -> *mut u8 {
    unsafe {
        let reuse = if candidate_count == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(candidates, candidate_count as usize)
        };
        let sources = core::slice::from_raw_parts(sources, count as usize);
        let len = sources
            .iter()
            .try_fold(0u32, |len, &array| {
                len.checked_add((*array.cast::<Array>()).len)
            })
            .expect("array length overflow");
        slot_bytes(len.max(MIN_CAPACITY));
        let eligible = |index: usize, array: *mut u8| {
            reuse.contains(&(index as u32))
                && sources.iter().filter(|&&other| other == array).count() == 1
        };
        let candidates = || {
            sources
                .iter()
                .copied()
                .enumerate()
                .filter(|&(index, array)| eligible(index, array))
        };
        let selected = candidates()
            .find(|&(_, array)| (*array.cast::<Array>()).cap >= len)
            .or_else(|| candidates().next());
        let target = selected
            .map_or_else(
                || perry_array_new_typed(len, references),
                |(_, array)| array,
            )
            .cast::<Array>();
        reserve(target, len, references);
        if let Some((index, _)) = selected {
            let prefix = sources[..index]
                .iter()
                .map(|&array| (*array.cast::<Array>()).len as usize)
                .sum::<usize>();
            core::ptr::copy(
                (*target).data,
                (*target).data.add(prefix),
                (*target).len as usize,
            );
        }
        let mut offset = 0;
        for &source in sources {
            let source = source.cast::<Array>();
            if source != target {
                core::ptr::copy_nonoverlapping(
                    (*source).data,
                    (*target).data.add(offset),
                    (*source).len as usize,
                );
            }
            offset += (*source).len as usize;
        }
        (*target).len = len;
        target.cast()
    }
}

/// Reserve live dense slots, retaining their reference shape.
unsafe fn reserve(array: *mut Array, len: u32, references: u32) {
    unsafe {
        if (*array).cap >= len {
            return;
        }
        let cap = len.max((*array).cap.saturating_mul(2).min(u32::MAX / SLOT));
        let data = crate::region::perry_buffer_resize(
            (*array).data.cast(),
            slot_bytes((*array).cap),
            slot_bytes(cap),
            slot_bytes((*array).len),
            element_shape(references),
        )
        .cast::<u64>();
        (*array).data = data;
        (*array).cap = cap;
    }
}

/// Append a snapshot of source slots, including explicit self-extension.
///
/// # Safety
/// Both operands are live arrays with identical element representation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_extend(target: *mut u8, source: *mut u8, references: u32) {
    unsafe {
        let (target, source) = (target.cast::<Array>(), source.cast::<Array>());
        let (before, added) = ((*target).len, (*source).len);
        let len = crate::sequence::length(before, added);
        slot_bytes(len);
        reserve(target, len, references);
        core::ptr::copy(
            (*source).data,
            (*target).data.add(before as usize),
            added as usize,
        );
        (*target).len = len;
    }
}

/// Combine normalized spread-call arguments, reserving their total size once.
///
/// # Safety
/// `segments` is an array of live arrays with the requested element representation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_concat_segments(
    segments: *mut u8,
    references: u32,
) -> *mut u8 {
    unsafe {
        let segments = &*segments.cast::<Array>();
        let sources = core::slice::from_raw_parts(segments.data, segments.len as usize);
        let len = sources
            .iter()
            .try_fold(0u32, |len, &source| {
                len.checked_add((*(source as usize as *const Array)).len)
            })
            .expect("array length overflow");
        slot_bytes(len);
        let result = perry_array_new_typed(len, references);
        for &source in sources {
            perry_array_extend(result, source as usize as *mut u8, references);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocate_array(values: &[u64], capacity: u32) -> *mut u8 {
        let array = perry_array_new_typed(capacity, 0).cast::<Array>();
        // SAFETY: capacity covers these dense scalar slots.
        unsafe {
            core::ptr::copy_nonoverlapping(values.as_ptr(), (*array).data, values.len());
            (*array).len = values.len() as u32;
        }
        array.cast()
    }

    #[test]
    fn concat_prefers_existing_capacity_and_grows_the_earliest_candidate() {
        crate::RuntimeInstance::default().enter(|| {
            crate::region::perry_task_enter();
            let sources = [
                allocate_array(&[10, 11, 12], 4),
                allocate_array(&[20, 21], 8),
                allocate_array(&[30], 16),
            ];
            // SAFETY: sources are exclusive scalar arrays with no external observers.
            unsafe {
                let result = perry_array_concat_many(sources.as_ptr(), 3, 0, [0, 1, 2].as_ptr(), 3);
                assert_eq!(result, sources[1]);
                assert_eq!(
                    core::slice::from_raw_parts((*result.cast::<Array>()).data, 6),
                    &[10, 11, 12, 20, 21, 30]
                );
                let sources = [
                    allocate_array(&[1, 2, 3], 4),
                    allocate_array(&[4, 5, 6], 4),
                    allocate_array(&[7, 8, 9], 4),
                ];
                let result = perry_array_concat_many(sources.as_ptr(), 3, 0, [0, 1, 2].as_ptr(), 3);
                assert_eq!(result, sources[0]);
                assert_eq!(
                    core::slice::from_raw_parts((*result.cast::<Array>()).data, 9),
                    &[1, 2, 3, 4, 5, 6, 7, 8, 9]
                );
            }
            crate::region::perry_task_free();
        });
    }

    #[test]
    fn concat_rejects_duplicates_and_supports_later_candidates_without_allocation() {
        crate::RuntimeInstance::default().enter(|| {
            crate::region::perry_task_enter();
            let repeated = allocate_array(&[1, 2], 8);
            // SAFETY: the helper must reject a duplicated candidate before mutation.
            unsafe {
                let result = perry_array_concat(repeated, repeated, 0, 3);
                assert_ne!(result, repeated);
                assert_eq!((*repeated.cast::<Array>()).len, 2);
                assert_eq!(
                    core::slice::from_raw_parts((*result.cast::<Array>()).data, 4),
                    &[1, 2, 1, 2]
                );
                let sources = (0..40)
                    .map(|value| allocate_array(&[value], if value == 39 { 64 } else { 4 }))
                    .collect::<Vec<_>>();
                let before = crate::roots::perry_gc_counter(0);
                let result = perry_array_concat_many(sources.as_ptr(), 40, 0, [39].as_ptr(), 1);
                assert_eq!(result, sources[39]);
                assert_eq!(crate::roots::perry_gc_counter(0), before);
                assert_eq!(
                    core::slice::from_raw_parts((*result.cast::<Array>()).data, 40),
                    &(0..40).collect::<Vec<u64>>()
                );
            }
            crate::region::perry_task_free();
        });
    }

    #[test]
    fn bulk_extension_snapshots_self_before_growth() {
        crate::RuntimeInstance::default().enter(|| {
            crate::region::perry_task_enter();
            let array = allocate_array(&[1, 2], 4);
            // SAFETY: explicit self-extension snapshots the original length and slots.
            unsafe {
                perry_array_extend(array, array, 0);
                perry_array_extend(array, array, 0);
                assert_eq!(
                    core::slice::from_raw_parts((*array.cast::<Array>()).data, 8),
                    &[1, 2, 1, 2, 1, 2, 1, 2]
                );
            }
            crate::region::perry_task_free();
        });
    }
}
