//! Checked sequence sizes shared by array slots and UTF-8 buffers.
#[inline(always)]
pub(crate) fn byte_size(elements: u32, stride: u32, overhead: u32) -> u32 {
    elements
        .checked_mul(stride)
        .and_then(|bytes| bytes.checked_add(overhead))
        .expect("sequence capacity overflow")
}
#[inline(always)]
pub(crate) fn length(left: u32, right: u32) -> u32 {
    left.checked_add(right).expect("sequence length overflow")
}
