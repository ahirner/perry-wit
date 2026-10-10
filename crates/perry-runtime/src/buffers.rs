//! Canonical storage stays pinned while the host lowers a value. Lifting transfers the
//! payload to an ordinary collectible owner, or releases it after a native-layout copy.
use core::ptr;

const HEADER: usize = 8;

/// Allocate a canonical buffer. No collection occurs while the host is lowering values.
///
/// # Safety
/// A non-null `old` is a canonical buffer with at least `old_size` readable bytes.
pub(crate) unsafe fn reallocate(old: *mut u8, old_size: u32, new_size: u32) -> *mut u8 {
    if new_size == 0 {
        unsafe { release(old) };
        return HEADER as *mut u8;
    }
    let size = new_size
        .checked_add(HEADER as u32)
        .expect("canonical buffer size");
    // SAFETY: an opaque block has no pointer fields. Header and payload are 8-aligned.
    unsafe {
        let owner = crate::roots::perry_gc_alloc(size, ptr::null());
        let token = crate::roots::pin_external(owner);
        owner.cast::<u32>().write(token);
        let data = owner.add(HEADER);
        if old as usize > HEADER && old_size != 0 {
            ptr::copy_nonoverlapping(old, data, old_size.min(new_size) as usize);
            crate::roots::transfer_copied(old_size.min(new_size));
            release(old);
        }
        data
    }
}

/// Transfer a canonical buffer from the host pin to its compiled owner, without copying.
///
/// # Safety
/// `data` came from [reallocate], or is the empty canonical pointer (0 or 8).
pub(crate) unsafe fn release(data: *mut u8) {
    if data as usize <= HEADER {
        return;
    }
    // SAFETY: header immediately precedes this canonical buffer; zero means already lifted.
    unsafe {
        let token = data.sub(HEADER).cast::<u32>();
        if *token != 0 {
            crate::roots::perry_gc_unpin(core::mem::replace(&mut *token, 0));
        }
    }
}
