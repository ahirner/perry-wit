//! Typed arrays with optional hole storage and 8-byte or 16-byte element slots.
//!
//! Layout, read and written inline by compiled code: `len: u32` at 0, `cap: u32` at 4,
//! `data: u32` at 8 and a nullable hole bitset at 12 ([`HOLES`]). Elements are scalars,
//! a 32-bit reference in the low half of a slot, or a
//! numeric tag and payload (see [`Elements`]).
//! Every array maintains `cap >= 1`, so compiled code can read slot 0 unconditionally and
//! select the result with branch-free bounds checks. Growth doubles capacity and preserves
//! live elements. Collectible arrays release old storage through tracing, while proven
//! bounded task allocations retain their storage until task completion.
use crate::region::perry_alloc;

pub(crate) use crate::layout::array::*;
const MIN_CAPACITY: u32 = 4;

#[cfg(target_arch = "wasm32")]
const _: () = {
    assert!(core::mem::size_of::<Array>() == 16);
    assert!(core::mem::offset_of!(Array, len) == LEN as usize);
    assert!(core::mem::offset_of!(Array, cap) == CAP as usize);
    assert!(core::mem::offset_of!(Array, data) == DATA as usize);
    assert!(core::mem::offset_of!(Array, holes) == HOLES as usize);
};

#[repr(C)]
struct Array {
    len: u32,
    cap: u32,
    data: *mut u64,
    holes: *mut u8,
}

impl Array {
    /// An empty array over `cap` slots at `data`, with length zero.
    fn empty(cap: u32, data: *mut u64) -> Self {
        Self {
            len: 0,
            cap,
            data,
            holes: core::ptr::null_mut(),
        }
    }
}

trait ElementStorage {
    fn bytes(self, cap: u32) -> u32;
    fn doubled(self, cap: u32) -> (u32, u32);
    fn shape(self) -> *const u32;
}

impl ElementStorage for Elements {
    /// Compute the byte size of `cap` slots, trapping if it exceeds the 32-bit address space.
    fn bytes(self, cap: u32) -> u32 {
        crate::byte_size(cap, self.stride(), 0)
    }

    /// Compute twice `cap` and the byte size of that many slots, trapping if either overflows.
    fn doubled(self, cap: u32) -> (u32, u32) {
        let cap = cap.checked_mul(2).expect("array capacity overflow");
        (cap, self.bytes(cap))
    }

    /// The element shape of a collectible array: precise reference slots or opaque numbers.
    fn shape(self) -> *const u32 {
        match self {
            Self::References => REFERENCE_SLOTS.as_ptr(),
            Self::Scalar | Self::Numeric => core::ptr::null(),
        }
    }
}

const ARRAY_SHAPE: [u32; 4] = [
    0,
    2,
    core::mem::offset_of!(Array, data) as u32,
    core::mem::offset_of!(Array, holes) as u32,
];
const REFERENCE_SLOTS: [u32; 3] = [SLOT, 1, 0];

/// A cleared hole bitset for `capacity` elements.
fn hole_storage(capacity: u32) -> *mut u8 {
    let bytes = hole_bytes(capacity);
    // SAFETY: the bitset holds no references and starts with every element present.
    unsafe {
        let holes = crate::region::perry_alloc_shaped(bytes, core::ptr::null());
        core::ptr::write_bytes(holes, 0, bytes as usize);
        holes
    }
}

/// Mark or clear `index` in the hole bitset `holes`.
///
/// # Safety
/// `holes` covers `index`.
unsafe fn set_hole(holes: *mut u8, index: u32, hole: bool) {
    let mask = 1 << (index % 8);
    unsafe {
        let byte = holes.add((index / 8) as usize);
        *byte = if hole { *byte | mask } else { *byte & !mask };
    }
}

/// Copy `count` bits of `source` (null for none) into `holes` from bit `start`.
///
/// # Safety
/// `holes` covers `start + count` bits and `source` is null or covers `count`. When both
/// share storage, the copied ranges are disjoint.
unsafe fn copy_holes(holes: *mut u8, start: u32, source: *const u8, count: u32) {
    for index in 0..count {
        unsafe { set_hole(holes, start + index, is_hole(source, index)) };
    }
}

unsafe fn resize_holes(array: *mut Array, capacity: u32) {
    unsafe {
        let holes = (*array).holes;
        if holes.is_null() {
            return;
        }
        let len = (*array).len;
        let holes = crate::region::perry_buffer_resize(
            holes,
            hole_bytes((*array).cap),
            hole_bytes(capacity),
            hole_bytes(len),
            core::ptr::null(),
        );
        // Bits from `len` may be stale after truncation; growth starts them present.
        if !len.is_multiple_of(8) {
            *holes.add((len / 8) as usize) &= (1u8 << (len % 8)) - 1;
        }
        let live = len.div_ceil(8);
        core::ptr::write_bytes(
            holes.add(live as usize),
            0,
            (hole_bytes(capacity) - live) as usize,
        );
        (*array).holes = holes;
    }
}

unsafe fn concat_holes(
    sources: impl Iterator<Item = *const Array> + Clone,
    capacity: u32,
) -> *mut u8 {
    unsafe {
        if sources.clone().all(|source| (*source).holes.is_null()) {
            return core::ptr::null_mut();
        }
        let holes = hole_storage(capacity);
        let mut offset = 0;
        for source in sources {
            if !(*source).holes.is_null() {
                copy_holes(holes, offset, (*source).holes, (*source).len);
            }
            offset += (*source).len;
        }
        holes
    }
}

/// Clear a slot, appending a hole when `index == length`.
///
/// # Safety
/// `array` is live with the given element representation and `index <= length`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_hole(array: *mut u8, index: u32, references: u32) {
    unsafe {
        let array = array.cast::<Array>();
        let elements = Elements::decode(references);
        assert!(index <= (*array).len);
        let len = (*array).len.max(crate::sequence_length(index, 1));
        reserve(array, len, elements);
        if (*array).holes.is_null() {
            (*array).holes = hole_storage((*array).cap);
        }
        set_hole((*array).holes, index, true);
        core::ptr::write_bytes(
            (*array).data.add(index as usize * elements.words()),
            0,
            elements.words(),
        );
        (*array).len = len;
    }
}

/// A new array with room for at least `capacity` elements and length 0.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_array_new(capacity: u32) -> *mut u8 {
    allocate(capacity, Elements::Scalar)
}

/// Double the capacity of `array` when a compiled push finds it full.
///
/// # Safety
/// `array` must come from [perry_array_new].
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_grow(array: *mut u8) {
    // SAFETY: forwarded from the caller; scalar arrays use scalar slots.
    unsafe { grow(array, Elements::Scalar) }
}

/// A non-collectible array of `elements` with room for at least `capacity` of them.
fn allocate(capacity: u32, elements: Elements) -> *mut u8 {
    let cap = capacity.max(MIN_CAPACITY);
    let array = perry_alloc(core::mem::size_of::<Array>() as u32) as *mut Array;
    let data = perry_alloc(elements.bytes(cap)) as *mut u64;
    // SAFETY: both blocks were just allocated with sufficient size and alignment.
    unsafe { array.write(Array::empty(cap, data)) };
    array as *mut u8
}

/// Double the capacity of a non-collectible array, copying its live elements.
///
/// # Safety
/// `array` is live and was allocated with `elements` slots.
unsafe fn grow(array: *mut u8, elements: Elements) {
    let array = array as *mut Array;
    // SAFETY: the caller passes a live array; the new block holds every old element.
    unsafe {
        let (cap, bytes) = elements.doubled((*array).cap);
        let data = perry_alloc(bytes) as *mut u64;
        core::ptr::copy_nonoverlapping(
            (*array).data,
            data,
            (*array).len as usize * elements.words(),
        );
        (*array).data = data;
        resize_holes(array, cap);
        (*array).cap = cap;
    }
}

/// Allocate an array whose header and element storage are collectible. Reference slots are
/// traced precisely, and numeric slots are treated as opaque values.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn perry_array_new_collectible(capacity: u32, references: u32) -> *mut u8 {
    let elements = Elements::decode(references);
    let cap = capacity.max(MIN_CAPACITY);
    // SAFETY: both static shapes describe these zeroed allocations exactly.
    unsafe {
        let array = crate::roots::perry_gc_alloc(
            core::mem::size_of::<Array>() as u32,
            ARRAY_SHAPE.as_ptr(),
        ) as *mut Array;
        let data = crate::roots::perry_gc_alloc(elements.bytes(cap), elements.shape()) as *mut u64;
        array.write(Array::empty(cap, data));
        array as *mut u8
    }
}

/// Grow collectible element storage.
///
/// # Safety
/// `array` was created by [perry_array_new_collectible], with the same reference flag.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_grow_collectible(array: *mut u8, references: u32) {
    let elements = Elements::decode(references);
    let array = array as *mut Array;
    // SAFETY: the caller owns a live array, and the copied slots retain their original shape.
    unsafe {
        let stride = elements.stride();
        let (cap, bytes) = elements.doubled((*array).cap);
        // The old capacity and length are at most half the new one, so their sizes fit.
        let data = crate::roots::resize_buffer(
            (*array).data.cast(),
            (*array).cap * stride,
            bytes,
            (*array).len * stride,
            elements.shape(),
        ) as *mut u64;
        (*array).data = data;
        resize_holes(array, cap);
        (*array).cap = cap;
    }
}

/// Create an array with a statically known element representation.
#[unsafe(no_mangle)]
pub extern "C" fn perry_array_new_typed(capacity: u32, references: u32) -> *mut u8 {
    #[cfg(feature = "collectible")]
    {
        perry_array_new_collectible(capacity, references)
    }
    #[cfg(not(feature = "collectible"))]
    {
        allocate(capacity, Elements::decode(references))
    }
}

/// Grow an array whose reference flag matches its construction.
///
/// # Safety
/// `array` is a live exclusively owned array created with the same `references` flag.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_array_grow_typed(array: *mut u8, references: u32) {
    #[cfg(feature = "collectible")]
    // SAFETY: forwarded from the caller.
    unsafe {
        perry_array_grow_collectible(array, references);
    }
    #[cfg(not(feature = "collectible"))]
    // SAFETY: forwarded from the caller.
    unsafe {
        grow(array, Elements::decode(references));
    }
}

/// Concatenate arrays, consuming only headers and buffers proved exclusive by MIR.
///
/// # Safety
/// Both arrays are live, have the same element representation, and every operand selected
/// by `reuse` is exclusively owned and unobserved after this call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_concat(
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
pub(crate) unsafe extern "C" fn perry_array_concat_many(
    sources: *const *mut u8,
    count: u32,
    references: u32,
    candidates: *const u32,
    candidate_count: u32,
) -> *mut u8 {
    let elements = Elements::decode(references);
    unsafe {
        let words = elements.words();
        let reuse = if candidate_count == 0 {
            &[]
        } else {
            core::slice::from_raw_parts(candidates, candidate_count as usize)
        };
        let sources = core::slice::from_raw_parts(sources, count as usize);
        let len = total_len(sources.iter().map(|&a| a.cast::<Array>().cast_const()));
        elements.bytes(len.max(MIN_CAPACITY));
        let mut selected = None;
        for (index, &array) in sources.iter().enumerate() {
            if reuse.contains(&(index as u32))
                && !sources[..index].contains(&array)
                && !sources[index + 1..].contains(&array)
            {
                selected.get_or_insert((index, array));
                if (*array.cast::<Array>()).cap >= len {
                    selected = Some((index, array));
                    break;
                }
            }
        }
        let target = selected
            .map_or_else(
                || perry_array_new_typed(len, references),
                |(_, array)| array,
            )
            .cast::<Array>();
        reserve(target, len, elements);
        (*target).holes = concat_holes(
            sources.iter().map(|&a| a.cast::<Array>().cast_const()),
            (*target).cap,
        );
        if let Some((index, _)) = selected {
            let prefix = sources[..index]
                .iter()
                .map(|&array| (*array.cast::<Array>()).len as usize)
                .sum::<usize>();
            core::ptr::copy(
                (*target).data,
                (*target).data.add(prefix * words),
                (*target).len as usize * words,
            );
        }
        copy_segments(
            target,
            len,
            words,
            sources.iter().map(|&a| a.cast::<Array>().cast_const()),
        )
    }
}

/// Reserve live dense slots, retaining their reference shape.
unsafe fn reserve(array: *mut Array, len: u32, elements: Elements) {
    unsafe {
        if (*array).cap >= len {
            return;
        }
        let cap = len.max(
            (*array)
                .cap
                .saturating_mul(2)
                .min(u32::MAX / elements.stride()),
        );
        let data = crate::region::perry_buffer_resize(
            (*array).data.cast(),
            elements.bytes((*array).cap),
            elements.bytes(cap),
            elements.bytes((*array).len),
            elements.shape(),
        )
        .cast::<u64>();
        (*array).data = data;
        resize_holes(array, cap);
        (*array).cap = cap;
    }
}

/// Append a snapshot of source slots, including explicit self-extension.
///
/// # Safety
/// Both operands are live arrays with identical element representation.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_extend(
    target: *mut u8,
    source: *mut u8,
    references: u32,
) {
    let elements = Elements::decode(references);
    unsafe {
        let words = elements.words();
        let (target, source) = (target.cast::<Array>(), source.cast::<Array>());
        let (before, added) = ((*target).len, (*source).len);
        let len = crate::sequence_length(before, added);
        elements.bytes(len);
        reserve(target, len, elements);
        if !(*source).holes.is_null() && (*target).holes.is_null() {
            (*target).holes = hole_storage((*target).cap);
        }
        if !(*target).holes.is_null() {
            // Self-extension reads bits below `before` and writes from `before`.
            copy_holes((*target).holes, before, (*source).holes, added);
        }
        core::ptr::copy(
            (*source).data,
            (*target).data.add(before as usize * words),
            added as usize * words,
        );
        (*target).len = len;
    }
}

/// Combine normalized spread-call arguments, reserving their total size once.
///
/// # Safety
/// `segments` is an array of live arrays with the requested element representation.
unsafe fn total_len(sources: impl IntoIterator<Item = *const Array>) -> u32 {
    sources
        .into_iter()
        .try_fold(0u32, |len, source| unsafe {
            len.checked_add((*source).len)
        })
        .expect("array length overflow")
}

unsafe fn copy_segments(
    target: *mut Array,
    len: u32,
    words: usize,
    sources: impl IntoIterator<Item = *const Array>,
) -> *mut u8 {
    unsafe {
        let mut offset = 0;
        for source in sources {
            let count = (*source).len as usize;
            if source != target {
                core::ptr::copy_nonoverlapping(
                    (*source).data,
                    (*target).data.add(offset * words),
                    count * words,
                );
            }
            offset += count;
        }
        (*target).len = len;
        target.cast()
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn perry_array_concat_segments(
    segments: *mut u8,
    references: u32,
) -> *mut u8 {
    let words = Elements::decode(references).words();
    unsafe {
        let segments = &*segments.cast::<Array>();
        let sources = core::slice::from_raw_parts(segments.data, segments.len as usize)
            .iter()
            .map(|&source| source as usize as *const Array);
        let len = total_len(sources.clone());
        let result = perry_array_new_typed(len, references).cast::<Array>();
        (*result).holes = concat_holes(sources.clone(), (*result).cap);
        copy_segments(result, len, words, sources)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

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
                assert!((*result.cast::<Array>()).holes.is_null());
                let sources = [allocate_array(&[10, 11], 4), allocate_array(&[20, 21], 8)];
                perry_array_hole(sources[0], 0, 0);
                perry_array_hole(sources[1], 1, 0);
                let result = perry_array_concat_many(sources.as_ptr(), 2, 0, [1].as_ptr(), 1);
                assert_eq!(result, sources[1]);
                assert_eq!(hole_bits(result, 4), [true, false, false, true]);
                assert_eq!(
                    core::slice::from_raw_parts((*result.cast::<Array>()).data, 4),
                    &[0, 11, 20, 0]
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

    /// The first `count` bits of `array`'s hole bitset.
    unsafe fn hole_bits(array: *mut u8, count: u32) -> Vec<bool> {
        let holes = unsafe { (*array.cast::<Array>()).holes };
        (0..count)
            .map(|index| unsafe { is_hole(holes, index) })
            .collect()
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
                assert!((*array.cast::<Array>()).holes.is_null());
                perry_array_hole(array, 1, 0);
                perry_array_extend(array, array, 0);
                let array = array.cast::<Array>();
                assert_eq!((*array).len, 16);
                assert_eq!(
                    hole_bits(array.cast(), 16),
                    (0..16).map(|index| index % 8 == 1).collect::<Vec<_>>()
                );
                assert_eq!(*(*array).data.add(1), 0);
                assert_eq!(*(*array).data.add(9), 0);
                perry_array_hole(array.cast(), 16, 0);
                assert_eq!((*array).len, 17);
                let cap = (*array).cap;
                assert!(is_hole((*array).holes, 16));
                // Truncation leaves bit 16 stale; growth must clear it as a present slot.
                (*array).len = 16;
                reserve(array, cap + 1, Elements::Scalar);
                assert!((16..(*array).cap).all(|index| !is_hole((*array).holes, index)));
            }
            crate::region::perry_task_free();
        });
    }
}
