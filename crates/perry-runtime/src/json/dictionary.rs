//! Native typed dictionaries and checked dictionary views of JSON object storage.
use super::*;
use crate::shape::Kind;

#[repr(C)]
pub(super) struct Dictionary {
    pub ty: *const Type,
    pub fields: *mut Object,
}

/// # Safety
/// `ty` is a static dictionary descriptor. The returned dictionary belongs to this task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_new(ty: *const Type) -> *mut u8 {
    let dictionary = perry_alloc(size_of::<Dictionary>() as u32).cast::<Dictionary>();
    // SAFETY: fresh storage has the header's size and alignment.
    unsafe {
        dictionary.write(Dictionary {
            ty,
            fields: Object::new(0),
        });
    }
    dictionary.cast()
}

/// # Safety
/// `reference` is a live native dictionary or a checked dictionary view; key is a live string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_get(reference: *mut u8, key: *mut u8) -> u64 {
    // SAFETY: tagged references point to views; native references have a dictionary header.
    unsafe {
        if let Some(source) = view::source(reference) {
            let view = &*((reference as usize & !1) as *const view::View);
            let element = (*view.ty).element();
            if !properties::has_own(*source.cast::<Json>(), string::text(key)) {
                return element.undefined();
            }
            return view::perry_json_unpack(element, perry_json_get(source, key));
        }
        let dictionary = &*reference.cast::<Dictionary>();
        let element = (*dictionary.ty).element();
        (*dictionary.fields).find(string::text(key)).map_or_else(
            || element.undefined(),
            |field| {
                let bits = (*field).value;
                if element.kind == Kind::Number {
                    crate::number_bits(f64::from_bits(bits))
                } else {
                    bits
                }
            },
        )
    }
}

/// # Safety
/// `reference` is a live dictionary/view; bits matches its static element type and key is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_set(reference: *mut u8, key: *mut u8, bits: u64) {
    // SAFETY: views pack with their target type and validate against source storage before writing.
    unsafe {
        if let Some(source) = view::source(reference) {
            let view = &*((reference as usize & !1) as *const view::View);
            let value = perry_json_pack((*view.ty).element(), bits);
            perry_json_set(source, key, value);
            return;
        }
        let dictionary = &*reference.cast::<Dictionary>();
        if string::text(key) == "__proto__" && (*dictionary.fields).find("__proto__").is_none() {
            failure(
                exception::TYPE_ERROR,
                "prototype setters are outside the closed-shape subset",
            );
            return;
        }
        (*dictionary.fields).set(key, bits);
    }
}

/// # Safety
/// The dictionary or view and the key string are live task references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_delete(reference: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: dictionary deletion leaves its stable header and every retained value untouched.
    unsafe {
        if let Some(source) = view::source(reference) {
            return perry_json_delete(source, key);
        }
        let dictionary = &*reference.cast::<Dictionary>();
        (*dictionary.fields).delete(string::text(key));
        1
    }
}

/// # Safety
/// The dictionary or view and the key string are live task references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_has(reference: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: own-property inspection does not read or convert the stored payload.
    unsafe {
        if let Some(source) = view::source(reference) {
            return perry_json_has_own(source, key);
        }
        let dictionary = &*reference.cast::<Dictionary>();
        (*dictionary.fields).find(string::text(key)).is_some() as u32
    }
}

/// # Safety
/// The dictionary or view is live; the returned key snapshot belongs to the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_keys(reference: *mut u8) -> *mut u8 {
    // SAFETY: only metadata is inspected, including dictionaries with non-JSON element types.
    unsafe {
        if let Some(source) = view::source(reference) {
            return perry_json_keys(source);
        }
        let dictionary = &*reference.cast::<Dictionary>();
        properties::keys(Json::Native {
            reference,
            ty: dictionary.ty,
        })
    }
}

/// # Safety
/// The dictionary or view is live. Each returned slot retains the dictionary's element type.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_values(reference: *mut u8) -> *mut u8 {
    // SAFETY: no guest code runs between the key snapshot and these checked reads.
    unsafe {
        let keys = perry_dictionary_keys(reference);
        let keys = array::slots(keys);
        let values = array::perry_array_new(keys.len() as u32);
        for (index, &key) in keys.iter().enumerate() {
            let bits = perry_dictionary_get(reference, key as usize as *mut u8);
            if exception::perry_exception_status() != 0 {
                return ptr::null_mut();
            }
            array::set_slot(values, index, bits);
        }
        values
    }
}
