//! Release unused canonical strings after native completion or cancellation acknowledgement.
use super::{Kind, Op, byte, wasi, word};

fn release_variant(value: *mut u8, other: u8) {
    if byte(value, 0) == other && byte(value, 4) != 0 {
        // SAFETY: the active `other(option<string>)` owns this canonical payload.
        unsafe { perry_rt_sys::release_canonical(word(value, 8) as usize as *mut u8) };
    }
}

/// The initialized result contains either an error string or a stat's descriptor-type string.
pub(super) fn release_result(op: &Op) {
    let payload = if op.kind == Kind::Stat {
        wasi::STAT_PAYLOAD
    } else {
        4
    };
    if byte(op.area, 0) != 0 {
        release_variant(op.area.wrapping_add(payload), wasi::ERROR_OTHER);
    } else if op.kind == Kind::Stat {
        release_variant(op.area.wrapping_add(payload), wasi::TYPE_OTHER);
    }
}

/// Directory names may have been adopted by native strings, and releasing their pins is idempotent.
pub(super) fn release_entries(contents: &[u8]) {
    for entry in contents.as_chunks::<{ wasi::ENTRY_SIZE }>().0 {
        let entry = entry.as_ptr().cast_mut();
        release_variant(entry, wasi::TYPE_OTHER);
        // SAFETY: each initialized entry owns its canonical name.
        unsafe {
            perry_rt_sys::release_canonical(word(entry, wasi::ENTRY_NAME) as usize as *mut u8)
        };
    }
}
