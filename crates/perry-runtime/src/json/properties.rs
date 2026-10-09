//! Own-property operations shared by parsed dictionaries and native JSON views.
use super::*;
use alloc::vec::Vec;

/// # Safety
/// `value` is undefined or a live JSON value. Aggregate keys are outside this subset.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_key(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return string::copy("undefined");
    }
    // SAFETY: the non-null value is live, and the variant selects its payload.
    unsafe {
        match *value.cast::<Json>() {
            Json::String(key) => key,
            Json::Null => string::copy("null"),
            Json::Boolean(value) => string::copy(if value { "true" } else { "false" }),
            Json::Number(value) => string::copy(ryu_js::Buffer::new().format(value)),
            _ => failure(
                exception::TYPE_ERROR,
                "object property keys must be primitive values",
            ),
        }
    }
}

pub(super) unsafe fn has_own(value: Json, key: &str) -> bool {
    // SAFETY: only matching variants reach their native storage accessor.
    unsafe {
        let length = match value {
            Json::Object(object) => return (*object).find(key).is_some(),
            Json::Native { reference, ty } if (*ty).kind == NativeKind::Dictionary => {
                return (*(*reference.cast::<dictionary::Dictionary>()).fields)
                    .find(key)
                    .is_some();
            }
            Json::Native { reference, ty } if (*ty).kind == NativeKind::Record => {
                return native::field(reference, &*ty, key).is_some();
            }
            Json::Array(array)
            | Json::Native {
                reference: array, ..
            } => array::slots(array).len(),
            Json::String(text) => (*text.cast::<string::Str>()).utf16 as usize,
            _ => return false,
        };
        key == "length" || canonical_index(key).is_some_and(|index| index < length)
    }
}

/// # Safety
/// The value is undefined or task-live; the key is a live string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_has_own(value: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: optional references are checked before their variant is read.
    unsafe {
        if value.is_null() || matches!(*value.cast::<Json>(), Json::Null) {
            failure(
                exception::TYPE_ERROR,
                "cannot inspect properties of null or undefined",
            );
            return 0;
        }
        has_own(*value.cast::<Json>(), string::text(key)) as u32
    }
}

/// # Safety
/// The value is undefined or task-live. The returned array owns a snapshot of the keys.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_keys(value: *mut u8) -> *mut u8 {
    // SAFETY: descriptors and headers identify initialized task storage; nothing is mutated.
    unsafe {
        if value.is_null() || matches!(*value.cast::<Json>(), Json::Null) {
            return failure(exception::TYPE_ERROR, "cannot enumerate null or undefined");
        }
        keys(*value.cast::<Json>())
    }
}

pub(super) unsafe fn keys(value: Json) -> *mut u8 {
    // SAFETY: the caller supplies a live aggregate or scalar; referenced keys stay task-live.
    unsafe {
        let mut keys = Vec::new();
        let length = match value {
            Json::Object(object) => {
                keys.extend(
                    (*object)
                        .fields()
                        .iter()
                        .filter(|field| !field.key.is_null())
                        .map(|field| field.key),
                );
                0
            }
            Json::Native { reference, ty } if (*ty).kind == NativeKind::Dictionary => {
                let object = (*reference.cast::<dictionary::Dictionary>()).fields;
                keys.extend(
                    (*object)
                        .fields()
                        .iter()
                        .filter(|field| !field.key.is_null())
                        .map(|field| field.key),
                );
                0
            }
            Json::Native { reference, ty } if (*ty).kind == NativeKind::Record => {
                let record = &*reference.cast::<Record>();
                keys.extend(
                    (0..(*ty).count as usize)
                        .map(|index| (*ty).fields()[*record.order.add(index) as usize].name),
                );
                0
            }
            Json::Array(array)
            | Json::Native {
                reference: array, ..
            } => array::slots(array).len(),
            Json::String(text) => (*text.cast::<string::Str>()).utf16 as usize,
            _ => 0,
        };
        let result = array::perry_array_new((keys.len() + length) as u32);
        if length != 0 {
            for index in 0..length {
                let key = string::copy(ryu_js::Buffer::new().format(index as f64));
                array::set_slot(result, index, key as usize as u64);
            }
        } else {
            keys.sort_by_key(|&key| {
                canonical_index(string::text(key))
                    .filter(|&index| index < u32::MAX as usize)
                    .unwrap_or(usize::MAX)
            });
            for (index, key) in keys.into_iter().enumerate() {
                array::set_slot(result, index, key as usize as u64);
            }
        }
        result
    }
}

/// # Safety
/// The value is task-live and the key is a live string. Native shapes and dense arrays
/// cannot lose existing properties; rejected deletions leave them unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_delete(value: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: variant checks precede mutation; dictionary deletion keeps its header stable.
    unsafe {
        let key = string::text(key);
        let value = *value.cast::<Json>();
        if let Json::Object(object) = value {
            (*object).delete(key);
            return 1;
        }
        if let Json::Native { reference, ty } = value
            && (*ty).kind == NativeKind::Dictionary
        {
            (*(*reference.cast::<dictionary::Dictionary>()).fields).delete(key);
            return 1;
        }
        let error = match value {
            Json::Null => Some((exception::TYPE_ERROR, "cannot delete a property of null")),
            Json::Native { ty, .. } if (*ty).kind == NativeKind::Record => {
                Some((exception::TYPE_ERROR, "cannot delete from a closed record"))
            }
            Json::Array(_) | Json::Native { .. } if key != "length" && has_own(value, key) => {
                Some((
                    exception::RANGE_ERROR,
                    "deleting an array element would leave a hole",
                ))
            }
            _ if has_own(value, key) => {
                Some((exception::TYPE_ERROR, "cannot delete this property"))
            }
            _ => None,
        };
        if let Some((kind, message)) = error {
            failure(kind, message);
            0
        } else {
            1
        }
    }
}
