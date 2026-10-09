//! Checked typed aliases of JSON aggregates. The low reference bit distinguishes a view
//! from aligned native storage; the JSON source remains the sole owner of its contents.
use super::*;
use crate::shape::Kind;

#[repr(C)]
pub(super) struct View {
    pub source: *mut u8,
    pub ty: *const Type,
}

/// # Safety
/// `reference` is a native aggregate or a tagged view produced by this module.
pub(crate) unsafe fn source(reference: *mut u8) -> Option<*mut u8> {
    if reference as usize & 1 == 0 {
        None
    } else {
        // SAFETY: removing the tag recovers the live, aligned view allocation.
        Some(unsafe { (*((reference as usize & !1) as *const View)).source })
    }
}

pub(super) fn identity(value: Json) -> *mut u8 {
    match value {
        Json::Array(reference) | Json::Native { reference, .. } => reference,
        Json::Object(object) => object.cast(),
        _ => ptr::null_mut(),
    }
}

/// # Safety
/// The reference is null, a native aggregate, or a live typed view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_reference_identity(reference: *mut u8) -> *mut u8 {
    // SAFETY: views always refer directly to a JSON aggregate, never another view.
    unsafe { source(reference).map_or(reference, |source| identity(*source.cast::<Json>())) }
}

/// Read aggregate members without allocating wrappers during validation.
unsafe fn member(value: Json, index: usize, key: Option<&str>) -> Option<Json> {
    // SAFETY: the variant identifies live storage and all indices are checked.
    unsafe {
        match value {
            Json::Object(object) => (*object)
                .find(key?)
                .and_then(|field| ((*field).value as usize as *const Json).as_ref().copied()),
            Json::Array(array) => array::slots(array)
                .get(index)
                .and_then(|bits| (*bits as usize as *const Json).as_ref().copied()),
            Json::Native { reference, ty } => {
                let slot = if (*ty).kind != Kind::Array {
                    native::field(reference, &*ty, key?)?
                } else {
                    let slots = array::slots(reference);
                    native::Slot {
                        address: slots.get(index)? as *const u64 as *mut u8,
                        ty: (*ty).element(),
                        key: ptr::null_mut(),
                    }
                };
                native::read(&*slot.ty, (*slot.ty).read(slot.address))
            }
            _ => None,
        }
    }
}

unsafe fn length(value: Json) -> Option<usize> {
    // SAFETY: only array variants reach the native dense-array accessor.
    unsafe {
        match value {
            Json::Array(array) => Some(array::slots(array).len()),
            Json::Native { reference, ty } if (*ty).kind == Kind::Array => {
                Some(array::slots(reference).len())
            }
            _ => None,
        }
    }
}

/// Validate the reachable shape without copying or changing its representation. Ancestor
/// pairs terminate recursive schemas and cycles; memoization also bounds shared DAG traversal.
unsafe fn matches(
    ty: &Type,
    value: Option<Json>,
    seen: &mut alloc::collections::BTreeSet<(usize, usize)>,
    depth: usize,
) -> bool {
    // SAFETY: all descriptors and JSON values come from live, checked aggregate members.
    unsafe {
        if ty.kind == Kind::Optional {
            return value.is_none() || matches(ty.element(), value, seen, depth);
        }
        let Some(value) = value else {
            return false;
        };
        match ty.kind {
            Kind::Json => true,
            Kind::Number => matches!(value, Json::Number(_)),
            Kind::Boolean => matches!(value, Json::Boolean(_)),
            Kind::String => matches!(value, Json::String(_)),
            Kind::Dictionary => {
                let children = match value {
                    Json::Object(object) => Children::Object((*object).fields().iter()),
                    Json::Native { reference, ty }
                        if matches!((*ty).kind, Kind::Record | Kind::Dictionary) =>
                    {
                        native::children(reference, &*ty)
                    }
                    _ => return false,
                };
                let pair = (ty as *const Type as usize, identity(value) as usize);
                if seen.contains(&pair) {
                    return true;
                }
                if depth == 128 {
                    return false;
                }
                seen.insert(pair);
                ty.fields().iter().all(|field| {
                    let key = string::text(field.name);
                    let present = properties::has_own(value, key);
                    if !present {
                        return field.optional;
                    }
                    matches(&*field.ty, member(value, 0, Some(key)), seen, depth + 1)
                }) && children.into_iter().all(|node| {
                    node.is_ok_and(|node| matches(ty.element(), node_value(node), seen, depth + 1))
                })
            }
            Kind::Array | Kind::Record => {
                let count = if ty.kind == Kind::Array {
                    let Some(count) = length(value) else {
                        return false;
                    };
                    count
                } else {
                    let object = match value {
                        Json::Object(_) => true,
                        Json::Native { ty, .. } => {
                            matches!((*ty).kind, Kind::Record | Kind::Dictionary)
                        }
                        _ => false,
                    };
                    if !object {
                        return false;
                    }
                    ty.count as usize
                };
                let pair = (ty as *const Type as usize, identity(value) as usize);
                if seen.contains(&pair) {
                    return true;
                }
                if depth == 128 {
                    return false;
                }
                seen.insert(pair);
                (0..count).all(|index| {
                    let (child, key) = if ty.kind == Kind::Array {
                        (ty.element(), None)
                    } else {
                        let field = &ty.fields()[index];
                        (&*field.ty, Some(string::text(field.name)))
                    };
                    matches(child, member(value, index, key), seen, depth + 1)
                })
            }
            _ => false,
        }
    }
}

/// # Safety
/// `ty` describes JSON-compatible storage and any references in `value` are task-live.
pub(super) unsafe fn accepts(ty: &Type, value: Option<Json>) -> bool {
    // SAFETY: matching only reads the live type graph and borrowed members.
    unsafe { matches(ty, value, &mut alloc::collections::BTreeSet::new(), 0) }
}

/// # Safety
/// `ty` and the optional JSON value are live. No native storage is changed on failure.
pub(super) unsafe fn convert(ty: &Type, value: *mut u8) -> Option<u64> {
    // SAFETY: validation precedes construction; views retain the original JSON identity.
    unsafe {
        if let Some(bits) = native::scalar_or_native(ty, value) {
            return Some(bits);
        }
        if ty.kind == Kind::Optional {
            return convert(ty.element(), value);
        }
        if !accepts(ty, value.cast::<Json>().as_ref().copied()) {
            return None;
        }
        let view = perry_alloc(size_of::<View>() as u32).cast::<View>();
        view.write(View { source: value, ty });
        Some((view as usize | 1) as u64)
    }
}

/// # Safety
/// `ty` describes a JSON-compatible target; `value` is null or a live JSON node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_unpack(ty: *const Type, value: *mut u8) -> u64 {
    // SAFETY: conversion validates the variant before returning a native value or view.
    unsafe { convert(&*ty, value) }.unwrap_or_else(|| {
        failure(
            exception::TYPE_ERROR,
            "JSON value does not match the requested type",
        );
        0
    })
}

/// # Safety
/// `reference` is a live native array or a typed array view.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_view_length(reference: *mut u8) -> u32 {
    // SAFETY: a view's source cannot change its aggregate kind.
    unsafe {
        source(reference).map_or_else(
            || array::slots(reference).len(),
            |source| length(*source.cast::<Json>()).expect("array view"),
        ) as u32
    }
}

/// # Safety
/// `reference` is a view; `index` identifies a declared field or an in-range array element.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_view_get(reference: *mut u8, index: u32) -> u64 {
    // SAFETY: the target descriptor supplies the checked conversion for the selected member.
    unsafe {
        let view = &*((reference as usize & !1) as *const View);
        let ty = &*view.ty;
        let (ty, value) = if ty.kind == Kind::Array {
            (ty.element(), perry_json_index(view.source, index as f64))
        } else {
            let field = &ty.fields()[index as usize];
            (&*field.ty, perry_json_get(view.source, field.name))
        };
        perry_json_unpack(ty, value)
    }
}

/// # Safety
/// `reference` is a view and `bits` matches the target field/element type. The array index
/// is dense, or the record index identifies a declared field.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_view_set(reference: *mut u8, index: u32, bits: u64) {
    // SAFETY: packing unwraps nested views; writes still check native source constraints.
    unsafe {
        let view = &*((reference as usize & !1) as *const View);
        let ty = &*view.ty;
        if ty.kind == Kind::Array {
            let value = perry_json_pack(ty.element(), bits);
            perry_json_set_index(view.source, index as f64, value);
        } else {
            let field = &ty.fields()[index as usize];
            let value = perry_json_pack(field.ty, bits);
            perry_json_set(view.source, field.name, value);
        }
    }
}
