//! Mutable byte storage shared by views. Transfer detaches the old buffer in constant time,
//! and each access checks its owner before reading the payload.
use core::ptr;

#[repr(C)]
pub struct ByteBuffer {
    pub data: *mut u8,
    pub len: u32,
    pub detached: bool,
}

/// Cached view bounds and a shared, detachable owner. Only the owner is a traced field.
#[repr(C)]
pub struct Bytes {
    pub len: u32,
    pub data: *mut u8,
    pub owner: *mut ByteBuffer,
    pub offset: u32,
}

impl Bytes {
    /// # Safety
    /// The owner is live for this view's lifetime.
    pub unsafe fn detached(&self) -> bool {
        unsafe { (*self.owner).detached }
    }
}

/// A new buffer owner over an allocation or a borrowed canonical payload.
pub fn bytes_view(len: u32, data: *mut u8) -> *mut u8 {
    let owner = object!(ByteBuffer; data);
    // SAFETY: freshly allocated owner; callers retain borrowed storage through acknowledgement.
    unsafe {
        owner.write(ByteBuffer {
            data,
            len,
            detached: false,
        });
        buffer_view(owner, 0, len)
    }
}

/// # Safety
/// The attached buffer contains `offset + len` initialized bytes.
pub unsafe fn buffer_view(owner: *mut ByteBuffer, offset: u32, len: u32) -> *mut u8 {
    let view = object!(Bytes; owner);
    unsafe {
        view.write(Bytes {
            len,
            data: (*owner).data.wrapping_add(offset as usize),
            owner,
            offset,
        });
    }
    view.cast()
}

/// A view retaining its source's owner.
///
/// # Safety
/// `source` is attached and `offset + len` is within its length.
pub unsafe fn bytes_subview(source: *mut u8, offset: u32, len: u32) -> *mut u8 {
    unsafe {
        let source = &*source.cast::<Bytes>();
        buffer_view(source.owner, source.offset + offset, len)
    }
}

/// Transfer ownership of an attached view's buffer in place, preserving view bounds.
///
/// # Safety
/// `source` is a live attached byte view.
pub unsafe fn bytes_transfer(source: *mut u8) -> *mut u8 {
    unsafe {
        let source = &*source.cast::<Bytes>();
        let owner = &mut *source.owner;
        let result = bytes_view(owner.len, owner.data);
        let result = &mut *result.cast::<Bytes>();
        result.offset = source.offset;
        result.data = result.data.wrapping_add(source.offset as usize);
        result.len = source.len;
        owner.data = ptr::null_mut();
        owner.len = 0;
        owner.detached = true;
        (result as *mut Bytes).cast()
    }
}

/// Refresh cached bounds before canonical lowering. Detached views lower as empty byte lists.
///
/// # Safety
/// `view` is a live byte view.
pub unsafe fn normalize_bytes(view: *mut u8) -> *mut u8 {
    unsafe {
        let value = &mut *view.cast::<Bytes>();
        if value.detached() {
            value.len = 0;
            value.data = ptr::NonNull::<u8>::dangling().as_ptr();
            value.offset = 0;
        }
    }
    view
}

/// The initialized bytes of an attached view, or an empty slice for a detached view.
///
/// # Safety
/// `view` and its owner stay live and unmodified while the slice is used.
pub unsafe fn view_bytes(view: *mut u8) -> &'static [u8] {
    unsafe {
        let view = &*view.cast::<Bytes>();
        if view.len == 0 || view.detached() {
            return &[];
        }
        core::slice::from_raw_parts(view.data, view.len as usize)
    }
}
