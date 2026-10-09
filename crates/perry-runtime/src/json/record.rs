//! Own-property operations for closed records, including optional field presence.
use super::*;

/// # Safety
/// `reference` is a live native record; `index` identifies an optional field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_record_include(reference: *mut u8, index: u32) {
    // SAFETY: optional records own mutable order tables with capacity for every field.
    unsafe {
        (*reference.cast::<Record>()).include(index);
    }
}

/// # Safety
/// The record/view and key string are live task references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_record_has(reference: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: native metadata identifies field slots; views retain live JSON source storage.
    unsafe {
        if let Some(source) = view::source(reference) {
            return perry_json_has_own(source, key);
        }
        let record = &*reference.cast::<Record>();
        native::field(reference, &*record.shape, string::text(key)).is_some() as u32
    }
}

/// # Safety
/// The record/view is live. The returned key snapshot belongs to the current task.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_record_keys(reference: *mut u8) -> *mut u8 {
    // SAFETY: enumerating names does not inspect or convert non-JSON field values.
    unsafe {
        if let Some(source) = view::source(reference) {
            return perry_json_keys(source);
        }
        properties::keys(Json::Native {
            reference,
            ty: (*reference.cast::<Record>()).shape,
        })
    }
}

/// # Safety
/// The record/view and key string are live task references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_record_delete(reference: *mut u8, key: *mut u8) -> u32 {
    // SAFETY: only optional fields have mutable presence and writable order storage.
    unsafe {
        let source = view::source(reference);
        let ty = if source.is_some() {
            &*(*((reference as usize & !1) as *const view::View)).ty
        } else {
            &*(*reference.cast::<Record>()).shape
        };
        let field = ty
            .fields()
            .iter()
            .enumerate()
            .find(|(_, field)| string::text(field.name) == string::text(key));
        if field.is_some_and(|(_, field)| !field.optional) {
            failure(
                exception::TYPE_ERROR,
                "cannot delete a required record field",
            );
            return 0;
        }
        if let Some(source) = source {
            return perry_json_delete(source, key);
        }
        if let Some((index, _)) = field {
            (*reference.cast::<Record>()).remove(index);
        }
        1
    }
}
