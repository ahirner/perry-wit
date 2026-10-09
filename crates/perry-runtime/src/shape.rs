//! Compiler-emitted type descriptors and native record layouts.
use core::mem::size_of;

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Opaque,
    Number,
    Boolean,
    String,
    Json,
    Array,
    Record,
    Optional,
    Dictionary,
}

/// `detail` points to an element/optional type, record fields, or a dictionary shape.
#[repr(C)]
pub struct Type {
    pub kind: Kind,
    pub detail: *const u8,
    pub count: u32,
}
#[repr(C)]
pub struct Field {
    pub name: *mut u8,
    pub ty: *const Type,
    pub optional: bool,
}
#[repr(C)]
pub struct DictionaryShape {
    pub element: *const Type,
    pub fields: *const Field,
}
/// Header preceding native 8-byte field slots. Order entries index the descriptor's fields.
#[repr(C)]
pub struct Record {
    pub shape: *const Type,
    pub order: *const u32,
}
pub const TYPE_SIZE: u32 = 12;
pub const FIELD_SIZE: u32 = 12;
pub const DICTIONARY_SHAPE_SIZE: u32 = 8;
pub const RECORD_SIZE: u32 = 8;
#[cfg(target_arch = "wasm32")]
const _: () = {
    assert!(size_of::<Type>() == TYPE_SIZE as usize);
    assert!(size_of::<Field>() == FIELD_SIZE as usize);
    assert!(size_of::<DictionaryShape>() == DICTIONARY_SHAPE_SIZE as usize);
    assert!(size_of::<Record>() == RECORD_SIZE as usize);
};

impl Type {
    /// # Safety
    /// This is a valid compiler-emitted descriptor of an array, dictionary or optional type.
    pub(crate) unsafe fn element(&self) -> &Self {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if self.kind == Kind::Dictionary {
                &*(*self.detail.cast::<DictionaryShape>()).element
            } else {
                &*self.detail.cast()
            }
        }
    }
    /// # Safety
    /// This is a valid compiler-emitted record or dictionary descriptor.
    pub(crate) unsafe fn fields(&self) -> &[Field] {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if self.count == 0 {
                return &[];
            }
            let fields = if self.kind == Kind::Dictionary {
                (*self.detail.cast::<DictionaryShape>()).fields
            } else {
                self.detail.cast()
            };
            core::slice::from_raw_parts(fields, self.count as usize)
        }
    }
    /// # Safety
    /// This descriptor and any optional element descriptors are valid.
    pub(crate) unsafe fn undefined(&self) -> u64 {
        match self.kind {
            Kind::Number => crate::UNDEFINED_NUMBER,
            Kind::Boolean => 2,
            // SAFETY: an optional's element descriptor is valid by the caller's contract.
            Kind::Optional => unsafe { self.element().undefined() },
            _ => 0,
        }
    }
    /// # Safety
    /// `slot` contains a live value of this type in native storage.
    pub(crate) unsafe fn read(&self, slot: *const u8) -> u64 {
        // SAFETY: reads only the initialized part of each eight-byte slot.
        unsafe {
            match self.kind {
                Kind::Optional => self.element().read(slot),
                Kind::Number => slot.cast::<u64>().read(),
                Kind::Boolean => slot.cast::<u32>().read() as u64,
                _ => slot.cast::<usize>().read() as u64,
            }
        }
    }
    /// # Safety
    /// `slot` has room for this type; `bits` has already been checked against it.
    pub(crate) unsafe fn write(&self, slot: *mut u8, bits: u64) {
        // SAFETY: preserves the native scalar/reference width.
        unsafe {
            match self.kind {
                Kind::Optional => self.element().write(slot, bits),
                Kind::Number => slot.cast::<u64>().write(bits),
                Kind::Boolean => slot.cast::<u32>().write(bits as u32),
                _ => slot.cast::<usize>().write(bits as usize),
            }
        }
    }
}
impl Record {
    /// # Safety
    /// The record and its shape are live. Optional records own a writable order table;
    /// unused entries contain `u32::MAX` after all present indices.
    pub(crate) unsafe fn order(&self) -> &[u32] {
        // SAFETY: the compiler allocates one order entry per declared field.
        unsafe {
            let count = (*self.shape).count as usize;
            if count == 0 {
                return &[];
            }
            let entries = core::slice::from_raw_parts(self.order, count);
            &entries[..entries
                .iter()
                .position(|&index| index == u32::MAX)
                .unwrap_or(count)]
        }
    }

    /// # Safety
    /// `index` names a declared field in this live record.
    pub(crate) unsafe fn contains(&self, index: usize) -> bool {
        // SAFETY: required fields are always present; optional presence comes from the order.
        unsafe { !(*self.shape).fields()[index].optional || self.order().contains(&(index as u32)) }
    }

    /// # Safety
    /// `index` names an optional field in a live record with writable order storage.
    pub(crate) unsafe fn include(&mut self, index: u32) {
        // SAFETY: a missing declared field guarantees free capacity in the order table.
        unsafe {
            let order = self.order();
            if order.contains(&index) {
                return;
            }
            self.order.cast_mut().add(order.len()).write(index);
        }
    }

    /// # Safety
    /// `index` names an optional field in a live record with writable order storage.
    pub(crate) unsafe fn remove(&mut self, index: usize) {
        // SAFETY: the order is a bounded initialized prefix and the slot has its declared type.
        unsafe {
            let order = self.order();
            let Some(position) = order.iter().position(|&field| field as usize == index) else {
                return;
            };
            let remaining = order.len() - position - 1;
            let slot = self.order.cast_mut().add(position);
            core::ptr::copy(slot.add(1), slot, remaining);
            slot.add(remaining).write(u32::MAX);
            let ty = &*(*self.shape).fields()[index].ty;
            ty.write(
                Self::slot((self as *mut Self).cast(), index),
                ty.undefined(),
            );
        }
    }

    /// # Safety
    /// `record` has an initialized slot for `index` after its header.
    pub(crate) unsafe fn slot(record: *mut u8, index: usize) -> *mut u8 {
        // SAFETY: checked by the caller against the descriptor.
        unsafe { record.add(size_of::<Self>() + 8 * index) }
    }
}
