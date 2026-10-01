//! NaN-boxing encodings and tag bit-manipulation utilities.

pub(crate) const STRING_TAG: u64 = 0x7FFF;
pub(crate) const POINTER_TAG: u64 = 0x7FFD;
pub(crate) const TAG_UNDEFINED: u64 = 0x7FFC_0000_0000_0001;
pub(crate) const TAG_NULL: u64 = 0x7FFC_0000_0000_0002;
pub(crate) const TAG_FALSE: u64 = 0x7FFC_0000_0000_0003;
pub(crate) const TAG_TRUE: u64 = 0x7FFC_0000_0000_0004;

pub(crate) fn nanbox_string(id: usize) -> i64 {
    let bits = (STRING_TAG << 48) | (id as u64 & 0xFFFF_FFFF);
    bits as i64
}

pub(crate) fn nanbox_pointer(id: usize) -> i64 {
    let bits = (POINTER_TAG << 48) | (id as u64 & 0xFFFF_FFFF);
    bits as i64
}

pub(crate) fn get_pointer_id(val: i64) -> Option<usize> {
    let bits = val as u64;
    if (bits >> 48) == POINTER_TAG {
        Some((bits & 0xFFFF_FFFF) as usize)
    } else {
        None
    }
}
