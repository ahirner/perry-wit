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

unsafe fn descriptor(reference: *mut u8) -> &'static Type {
    // SAFETY: aligned references are native headers; tagged references are checked views.
    unsafe {
        if reference as usize & 1 != 0 {
            &*(*((reference as usize & !1) as *const view::View)).ty
        } else {
            &*(*reference.cast::<Dictionary>()).ty
        }
    }
}

unsafe fn field(ty: &Type, key: *mut u8) -> Option<&crate::shape::Field> {
    // SAFETY: dictionary metadata and key strings remain live throughout the task.
    unsafe {
        ty.fields()
            .iter()
            .find(|field| string::text(field.name) == string::text(key))
    }
}

unsafe fn accepts_bits(element: &Type, field: &Type, bits: u64) -> bool {
    // SAFETY: the bits match the element type. Optional removal only tests its sentinel,
    // including non-JSON references; remaining refinements have JSON-compatible types.
    unsafe {
        if core::ptr::eq(element, field) {
            return true;
        }
        if element.kind == Kind::Optional && core::ptr::eq(element.element(), field) {
            return bits != element.undefined();
        }
        view::accepts(field, native::read(element, bits))
    }
}

unsafe fn accepts_field(ty: &Type, key: *mut u8, value: *mut u8) -> bool {
    // SAFETY: fields constrain the dictionary's element type, which codegen has verified.
    unsafe {
        field(ty, key)
            .is_none_or(|field| view::accepts(&*field.ty, value.cast::<Json>().as_ref().copied()))
    }
}

/// # Safety
/// `reference` is a live native dictionary or a checked dictionary view; key is a live string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_get(reference: *mut u8, key: *mut u8) -> u64 {
    // SAFETY: descriptor and source agree on the uniform element representation.
    unsafe {
        let ty = descriptor(reference);
        let element = ty.element();
        let source = view::source(reference);
        let stored = if source.is_none() {
            (*(*reference.cast::<Dictionary>()).fields).find(string::text(key))
        } else {
            None
        };
        let present = source.map_or(stored.is_some(), |source| {
            properties::has_own(*source.cast::<Json>(), string::text(key))
        });
        let constraint = field(ty, key);
        if !present {
            if constraint.is_some_and(|field| !field.optional) {
                failure(
                    exception::TYPE_ERROR,
                    "dictionary is missing a required field",
                );
            }
            return element.undefined();
        }
        if let Some(source) = source {
            let value = perry_json_get(source, key);
            if !accepts_field(ty, key, value) {
                failure(
                    exception::TYPE_ERROR,
                    "dictionary value does not match its named field",
                );
                return element.undefined();
            }
            return view::perry_json_unpack(element, value);
        }
        let bits = (*stored.expect("present native entry")).value;
        if constraint.is_some_and(|field| !accepts_bits(element, &*field.ty, bits)) {
            failure(
                exception::TYPE_ERROR,
                "dictionary value does not match its named field",
            );
            return element.undefined();
        }
        if element.kind == Kind::Number {
            crate::number_bits(f64::from_bits(bits))
        } else {
            bits
        }
    }
}

/// # Safety
/// `reference` is a live dictionary/view; bits matches its static element type and key is live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_dictionary_set(reference: *mut u8, key: *mut u8, bits: u64) {
    // SAFETY: validation finishes before the source storage changes, including native aliases.
    unsafe {
        let ty = descriptor(reference);
        if let Some(source) = view::source(reference) {
            let value = perry_json_pack(ty.element(), bits);
            if !accepts_field(ty, key, value) {
                failure(
                    exception::TYPE_ERROR,
                    "dictionary assignment does not match its named field",
                );
                return;
            }
            perry_json_set(source, key, value);
            return;
        }
        if field(ty, key).is_some_and(|field| !accepts_bits(ty.element(), &*field.ty, bits)) {
            failure(
                exception::TYPE_ERROR,
                "dictionary assignment does not match its named field",
            );
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
        if field(descriptor(reference), key).is_some_and(|field| !field.optional) {
            failure(
                exception::TYPE_ERROR,
                "cannot delete a required dictionary field",
            );
            return 0;
        }
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
