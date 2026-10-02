//! Shared byte storage and Uint8Array / Buffer views for the guest runtime.

use std::cell::RefCell;
use std::rc::Rc;

/// Shared backing storage for byte buffers.
#[derive(Clone, Debug)]
pub(crate) struct BufferStorage {
    pub(crate) data: Rc<RefCell<Vec<u8>>>,
}

impl BufferStorage {
    /// Allocate zero-filled storage of the given size.
    pub(crate) fn new(size: usize) -> Self {
        Self {
            data: Rc::new(RefCell::new(vec![0u8; size])),
        }
    }

    /// Allocate storage initialized with existing bytes.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            data: Rc::new(RefCell::new(bytes)),
        }
    }
}

/// A contiguous window into a `BufferStorage`.
#[derive(Clone, Debug)]
pub(crate) struct Uint8ArrayView {
    pub(crate) storage: BufferStorage,
    pub(crate) byte_offset: usize,
    pub(crate) byte_length: usize,
}

impl Uint8ArrayView {
    /// Create a new Uint8Array view of specified size backed by zero-filled storage.
    pub(crate) fn new(size: usize) -> Self {
        Self {
            storage: BufferStorage::new(size),
            byte_offset: 0,
            byte_length: size,
        }
    }

    /// Create a new Uint8Array view copying existing bytes.
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Self {
        let len = bytes.len();
        Self {
            storage: BufferStorage::from_bytes(bytes),
            byte_offset: 0,
            byte_length: len,
        }
    }

    /// Read a single byte at the logical index, if within view bounds.
    pub(crate) fn get(&self, index: usize) -> Option<u8> {
        if index < self.byte_length {
            let data = self.storage.data.borrow();
            let actual_idx = self.byte_offset + index;
            data.get(actual_idx).copied()
        } else {
            None
        }
    }

    /// Write a single byte at the logical index, if within view bounds.
    pub(crate) fn set(&self, index: usize, val: u8) {
        if index < self.byte_length {
            let mut data = self.storage.data.borrow_mut();
            let actual_idx = self.byte_offset + index;
            if actual_idx < data.len() {
                data[actual_idx] = val;
            }
        }
    }

    /// Return a copy of the slice bytes currently visible in this view.
    pub(crate) fn to_vec(&self) -> Vec<u8> {
        let data = self.storage.data.borrow();
        let start = self.byte_offset.min(data.len());
        let end = (self.byte_offset + self.byte_length).min(data.len());
        if end > start {
            data[start..end].to_vec()
        } else {
            Vec::new()
        }
    }

    /// Create a subview sharing the same backing storage.
    /// Mutations to overlapping regions will be visible across both views.
    pub(crate) fn subview(&self, start: f64, end: Option<f64>) -> Self {
        let start = relative_bound(start, self.byte_length);
        let end = end.map_or(self.byte_length, |end| {
            relative_bound(end, self.byte_length)
        });
        let len = end.saturating_sub(start);
        Self {
            storage: self.storage.clone(),
            byte_offset: self.byte_offset + start,
            byte_length: len,
        }
    }
}

/// Resolves ToIntegerOrInfinity bounds against the current view, before clamping.
fn relative_bound(value: f64, length: usize) -> usize {
    let integer = if value.is_nan() { 0.0 } else { value.trunc() };
    let relative = if integer < 0.0 {
        length as f64 + integer
    } else {
        integer
    };
    relative.clamp(0.0, length as f64) as usize
}
