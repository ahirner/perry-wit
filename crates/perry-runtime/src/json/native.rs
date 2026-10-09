//! Borrowed JSON views over native typed storage; mutations validate before writing.
use super::*;
use crate::shape::{Kind, Record, Type};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Slot {
    pub address: *mut u8,
    pub ty: *const Type,
    pub key: *mut u8,
}

/// # Safety
/// `ty` describes the initialized native value represented by `bits`.
pub(super) unsafe fn read(ty: &Type, bits: u64) -> Option<Json> {
    // SAFETY: descriptors and any referenced values are live for the task.
    unsafe {
        if matches!(ty.kind, Kind::Array | Kind::Record)
            && let Some(source) = view::source(bits as usize as *mut u8)
        {
            return Some(*source.cast::<Json>());
        }
        Some(match ty.kind {
            Kind::Optional => {
                let inner = ty.element();
                let absent = match inner.kind {
                    Kind::Number => bits == crate::UNDEFINED_NUMBER,
                    Kind::Boolean => bits == 2,
                    _ => bits == 0,
                };
                return if absent { None } else { read(inner, bits) };
            }
            Kind::Number => Json::Number(f64::from_bits(bits)),
            Kind::Boolean => Json::Boolean(bits != 0),
            Kind::String => Json::String(bits as usize as *mut u8),
            Kind::Json => return (bits != 0).then(|| *(bits as usize as *const Json)),
            Kind::Array if ty.element().kind == Kind::Json => Json::Array(bits as usize as *mut u8),
            Kind::Array => Json::Native {
                reference: bits as usize as *mut u8,
                ty,
            },
            Kind::Record => {
                let reference = bits as usize as *mut u8;
                Json::Native {
                    reference,
                    ty: (*reference.cast::<Record>()).shape,
                }
            }
            Kind::Opaque => unreachable!("non-JSON type reached JSON packing"),
        })
    }
}

/// # Safety
/// `ty` is a static valid descriptor; every non-null JSON reference is live.
pub(super) unsafe fn scalar_or_native(ty: &Type, value: *mut u8) -> Option<u64> {
    // SAFETY: descriptor and variant checks precede payload reads.
    unsafe {
        if ty.kind == Kind::Optional {
            let inner = ty.element();
            return if value.is_null() {
                Some(match inner.kind {
                    Kind::Number => crate::UNDEFINED_NUMBER,
                    Kind::Boolean => 2,
                    _ => 0,
                })
            } else {
                scalar_or_native(inner, value)
            };
        }
        if value.is_null() {
            return None;
        }
        if ty.kind == Kind::Json {
            return Some(value as usize as u64);
        }
        match (ty.kind, *value.cast::<Json>()) {
            (Kind::Number, Json::Number(value)) => Some(value.to_bits()),
            (Kind::Boolean, Json::Boolean(value)) => Some(value as u64),
            (Kind::String, Json::String(value)) => Some(value as usize as u64),
            (Kind::Array, Json::Array(value)) if ty.element().kind == Kind::Json => {
                Some(value as usize as u64)
            }
            (
                Kind::Array | Kind::Record,
                Json::Native {
                    reference,
                    ty: actual,
                },
            ) if core::ptr::eq(ty, actual) => Some(reference as usize as u64),
            _ => None,
        }
    }
}

/// # Safety
/// `reference` is native storage described by `ty`; `key` is valid UTF-8.
pub(super) unsafe fn field(reference: *mut u8, ty: &Type, key: &str) -> Option<Slot> {
    // SAFETY: native record field descriptors and storage agree.
    unsafe {
        ty.fields().iter().enumerate().find_map(|(index, field)| {
            (string::text(field.name) == key).then(|| Slot {
                address: Record::slot(reference, index),
                ty: field.ty,
                key: field.name,
            })
        })
    }
}

/// # Safety
/// The native aggregate is described by `ty` and belongs to the current task.
pub(super) unsafe fn get(reference: *mut u8, ty: &Type, key: &str) -> *mut u8 {
    // SAFETY: descriptor checks select the matching native layout.
    unsafe {
        if ty.kind == Kind::Record {
            return field(reference, ty, key).map_or(ptr::null_mut(), |slot| {
                perry_json_pack(slot.ty, (*slot.ty).read(slot.address))
            });
        }
        if key == "length" {
            return perry_json_number(array::slots(reference).len() as f64);
        }
        canonical_index(key).map_or(ptr::null_mut(), |index| {
            self::index(reference, ty, index as f64)
        })
    }
}

/// # Safety
/// The native aggregate is described by `ty` and belongs to the current task.
pub(super) unsafe fn index(reference: *mut u8, ty: &Type, index: f64) -> *mut u8 {
    // SAFETY: the array index is checked before the initialized element is read.
    unsafe {
        if ty.kind == Kind::Record {
            let mut buffer = ryu_js::Buffer::new();
            return get(reference, ty, buffer.format(index));
        }
        let slots = array::slots(reference);
        if index >= 0.0 && index < slots.len() as f64 && libm::trunc(index) == index {
            let element = ty.element();
            perry_json_pack(
                element,
                element.read(slots.as_ptr().add(index as usize).cast()),
            )
        } else {
            ptr::null_mut()
        }
    }
}

/// # Safety
/// Native storage and replacement belong to the current live task.
pub(super) unsafe fn set_key(reference: *mut u8, ty: &Type, key: &str, replacement: *mut u8) {
    // SAFETY: the shape lookup or dense index check validates the destination.
    unsafe {
        if ty.kind == Kind::Array {
            if let Some(index) = canonical_index(key) {
                set_index(reference, ty, index as f64, replacement);
                return;
            }
        } else if let Some(slot) = field(reference, ty, key) {
            set(slot, replacement);
            return;
        }
    }
    failure(
        exception::TYPE_ERROR,
        "JSON property assignment requires an existing object field",
    );
}

/// # Safety
/// The slot and replacement belong to this live task, and the slot has its declared type.
pub(super) unsafe fn set(slot: Slot, replacement: *mut u8) {
    // SAFETY: conversion completes before native storage is changed.
    unsafe {
        if let Some(bits) = view::convert(&*slot.ty, replacement) {
            (*slot.ty).write(slot.address, bits);
        } else {
            failure(
                exception::TYPE_ERROR,
                "JSON assignment does not match native storage type",
            );
        }
    }
}

/// # Safety
/// `reference` is a native array described by `ty`; replacement is task-live.
pub(super) unsafe fn set_index(reference: *mut u8, ty: &Type, index: f64, replacement: *mut u8) {
    // SAFETY: dense bounds and the element type are checked before any mutation or growth.
    unsafe {
        let length = array::slots(reference).len();
        if index < 0.0 || index > length as f64 || libm::trunc(index) != index || index.is_nan() {
            failure(
                exception::RANGE_ERROR,
                "JSON array write would leave a hole",
            );
        } else if let Some(bits) = view::convert(ty.element(), replacement) {
            array::set_slot(reference, index as usize, bits);
        } else {
            failure(
                exception::TYPE_ERROR,
                "JSON assignment does not match native storage type",
            );
        }
    }
}

/// # Safety
/// The native storage matches the descriptor and remains immutable while iterating.
pub(super) unsafe fn children(reference: *mut u8, ty: &Type) -> Children {
    // SAFETY: array headers and record shape metadata are live and initialized.
    unsafe {
        match ty.kind {
            Kind::Array => {
                let slots = array::slots(reference);
                Children::NativeArray {
                    next: slots.as_ptr().cast(),
                    remaining: slots.len(),
                    ty: ty.element(),
                }
            }
            Kind::Record => Children::NativeObject {
                reference,
                next: 0,
                ty,
            },
            _ => unreachable!("native JSON aggregates are arrays or records"),
        }
    }
}

/// Convert an unboxed scalar or borrow a native aggregate as JSON.
///
/// # Safety
/// `ty` is compiler-emitted metadata matching `bits`; all referenced storage is task-live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_pack(ty: *const Type, bits: u64) -> *mut u8 {
    // SAFETY: the generated call supplies the verified MIR operand's type.
    unsafe {
        if (*ty).kind == Kind::Json {
            return bits as usize as *mut u8;
        }
        read(&*ty, bits).map_or(ptr::null_mut(), allocate)
    }
}
