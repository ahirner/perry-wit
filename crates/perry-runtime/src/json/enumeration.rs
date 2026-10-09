//! Enumeration snapshots preserve unboxed slots and referenced aggregate identities.
use super::*;
use crate::{bytes, shape::Kind};

enum Member {
    Native { ty: *const Type, bits: u64 },
    Json(*mut u8),
    String(*mut u8),
    Number(f64),
}

unsafe fn keys(ty: &Type, bits: u64) -> *mut u8 {
    let reference = bits as usize as *mut u8;
    // SAFETY: the verified source type identifies the live payload and its representation.
    unsafe {
        match ty.kind {
            Kind::Json => perry_json_keys(reference),
            Kind::Record => record::perry_record_keys(reference),
            Kind::Dictionary => dictionary::perry_dictionary_keys(reference),
            Kind::Array | Kind::Tuple => {
                if let Some(source) = view::source(reference) {
                    perry_json_keys(source)
                } else {
                    properties::keys(Json::Native { reference, ty })
                }
            }
            Kind::String => properties::keys(Json::String(reference)),
            Kind::Bytes => {
                let length = bytes::perry_bytes_length(reference) as u32;
                let keys = array::perry_array_new(length);
                for index in 0..length {
                    let key = string::copy(ryu_js::Buffer::new().format(index as f64));
                    array::set_slot(keys, index as usize, key as usize as u64);
                }
                keys
            }
            Kind::TextOrBytes => {
                unreachable!("union enumeration must dispatch to its native alternative")
            }
            Kind::Number | Kind::Boolean | Kind::Opaque => array::perry_array_new(0),
            Kind::Optional => unreachable!("enumeration unwraps its source"),
        }
    }
}

unsafe fn string_member(reference: *mut u8, byte_offset: &mut usize) -> Member {
    // SAFETY: keys visit string indices in order; the offset remains at a UTF-8 boundary.
    unsafe {
        let character = string::text(reference)[*byte_offset..]
            .chars()
            .next()
            .expect("string enumeration index");
        let start = *byte_offset;
        *byte_offset += character.len_utf8();
        Member::String(string::copy(&string::text(reference)[start..*byte_offset]))
    }
}

unsafe fn member(ty: &Type, bits: u64, key: *mut u8, byte_offset: &mut usize) -> Member {
    let reference = bits as usize as *mut u8;
    // SAFETY: keys are an own-property snapshot and no guest callback can mutate the source.
    unsafe {
        match ty.kind {
            Kind::Json => {
                if let Json::String(text) = *reference.cast::<Json>() {
                    string_member(text, byte_offset)
                } else {
                    Member::Json(perry_json_get(reference, key))
                }
            }
            Kind::Dictionary => Member::Native {
                ty: ty.element(),
                bits: dictionary::perry_dictionary_get(reference, key),
            },
            Kind::Record => {
                if let Some((index, field)) = ty
                    .fields()
                    .iter()
                    .enumerate()
                    .find(|(_, field)| string::text(field.name) == string::text(key))
                {
                    let bits = if view::source(reference).is_some() {
                        view::perry_view_get(reference, index as u32)
                    } else {
                        (*field.ty).read(Record::slot(reference, index))
                    };
                    Member::Native { ty: field.ty, bits }
                } else {
                    Member::Json(perry_json_get(
                        view::source(reference).expect("extra field in parsed view"),
                        key,
                    ))
                }
            }
            Kind::Array | Kind::Tuple => {
                let index = canonical_index(string::text(key)).expect("array enumeration index");
                if let Some(element) = ty.indexed_element(index) {
                    let bits = if view::source(reference).is_some() {
                        view::perry_view_get(reference, index as u32)
                    } else {
                        element.read(array::slots(reference).as_ptr().add(index).cast())
                    };
                    Member::Native { ty: element, bits }
                } else {
                    Member::Json(perry_json_get(
                        view::source(reference).expect("extra element in parsed tuple view"),
                        key,
                    ))
                }
            }
            Kind::String => string_member(reference, byte_offset),
            Kind::Bytes => Member::Number(bytes::perry_bytes_get(
                reference,
                canonical_index(string::text(key)).expect("byte index") as f64,
            )),
            _ => unreachable!("non-enumerable primitive has no keys"),
        }
    }
}

unsafe fn convert_native(target: &Type, source: &Type, bits: u64) -> Option<u64> {
    // SAFETY: bits match source. Optional promotion/removal preserves the underlying slot width.
    unsafe {
        if core::ptr::eq(target, source) {
            return Some(if source.kind == Kind::Number {
                crate::number_bits(f64::from_bits(bits))
            } else {
                bits
            });
        }
        if target.kind == Kind::Optional {
            if source.kind == Kind::Optional && bits == source.undefined() {
                return Some(target.undefined());
            }
            return convert_native(
                target.element(),
                if source.kind == Kind::Optional {
                    source.element()
                } else {
                    source
                },
                bits,
            );
        }
        if source.kind == Kind::Optional {
            return if bits == source.undefined() {
                None
            } else {
                convert_native(target, source.element(), bits)
            };
        }
        view::convert(target, perry_json_pack(source, bits))
    }
}

unsafe fn convert_member(target: &Type, member: Member) -> Option<u64> {
    // SAFETY: each member variant contains a live typed slot or a live JSON/string reference.
    unsafe {
        match member {
            Member::Native { ty, bits } => convert_native(target, &*ty, bits),
            Member::Json(value) => view::convert(target, value),
            Member::String(value) => {
                let inner = if target.kind == Kind::Optional {
                    target.element()
                } else {
                    target
                };
                if inner.kind == Kind::String {
                    Some(value as usize as u64)
                } else {
                    view::convert(target, perry_json_string(value))
                }
            }
            Member::Number(value) => {
                let inner = if target.kind == Kind::Optional {
                    target.element()
                } else {
                    target
                };
                if inner.kind == Kind::Number {
                    Some(crate::number_bits(value))
                } else {
                    view::convert(target, perry_json_number(value))
                }
            }
        }
    }
}

/// # Safety
/// Descriptors are compiler-emitted; bits match source. Action is keys=0, values=1 or entries=2.
/// `element` is the result array's element type, including the tuple type for entries.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_enumerate(
    source: *const Type,
    bits: u64,
    element: *const Type,
    action: u32,
) -> *mut u8 {
    // SAFETY: type checks precede payload reads; output storage belongs to the current task.
    unsafe {
        let mut source = &*source;
        while source.kind == Kind::Optional {
            if bits == source.undefined() {
                return failure(exception::TYPE_ERROR, "cannot enumerate undefined");
            }
            source = source.element();
        }
        let keys = keys(source, bits);
        if exception::perry_exception_status() != 0 || action == 0 {
            return keys;
        }
        let target = if action == 2 {
            (*element).indexed_element(1).expect("entry value type")
        } else {
            &*element
        };
        let keys = array::slots(keys);
        let result = array::perry_array_new(keys.len() as u32);
        let mut byte_offset = 0;
        for (index, &key) in keys.iter().enumerate() {
            let value = member(source, bits, key as usize as *mut u8, &mut byte_offset);
            if exception::perry_exception_status() != 0 {
                return ptr::null_mut();
            }
            let Some(value) = convert_member(target, value) else {
                return failure(
                    exception::TYPE_ERROR,
                    "enumerated value does not match its declared type",
                );
            };
            let value = if action == 2 {
                let pair = array::perry_array_new(2);
                array::set_slot(pair, 0, key);
                array::set_slot(pair, 1, value);
                pair as usize as u64
            } else {
                value
            };
            array::set_slot(result, index, value);
        }
        result
    }
}
