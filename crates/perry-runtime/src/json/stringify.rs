//! Serialization controls prepared once, then shared by the pure measurement and write passes.
use super::*;
use alloc::{collections::BTreeSet, vec::Vec};
use perry_json_codec::StringifyOptions;

unsafe fn property_list(replacer: *mut u8) -> Result<Option<Vec<&'static str>>, Error> {
    let Value::Array(children) = RuntimeTree.value(Node::Value(replacer))? else {
        return Ok(None);
    };
    let mut keys = Vec::new();
    let mut seen = BTreeSet::new();
    for child in children {
        // SAFETY: children borrow the live replacer array, which guest code cannot mutate here.
        let key = unsafe {
            match node_value(child?) {
                Some(Json::String(key)) => key,
                Some(Json::Number(value)) => string::copy(ryu_js::Buffer::new().format(value)),
                _ => continue,
            }
        };
        // SAFETY: strings are borrowed task values or fresh task-owned numeric spellings.
        let key = unsafe { string::text(key) };
        if seen.insert(key) {
            keys.push(key);
        }
    }
    Ok(Some(keys))
}

pub(super) unsafe fn gap(space: *mut u8) -> Result<&'static str, Error> {
    // SAFETY: the space argument is undefined or a live JSON value.
    match unsafe { node_value(Node::Value(space)) } {
        Some(Json::Number(value)) => Ok(&"          "[..value.clamp(0.0, 10.0) as usize]),
        Some(Json::String(value)) => {
            // SAFETY: the variant holds a valid task string.
            let text = unsafe { string::text(value) };
            let mut units = 0;
            for (offset, scalar) in text.char_indices() {
                if units == 10 {
                    return Ok(&text[..offset]);
                }
                units += scalar.len_utf16();
                if units > 10 {
                    return Err(Error::UnpairedSurrogate(offset));
                }
            }
            Ok(text)
        }
        _ => Ok(""),
    }
}

pub(super) fn stringify_error(error: Error) -> *mut u8 {
    match error {
        Error::Capacity => failure(exception::RANGE_ERROR, "JSON text is too large"),
        Error::UnpairedSurrogate(_) => failure(
            exception::TYPE_ERROR,
            "JSON indentation would contain an unpaired surrogate",
        ),
        Error::PrototypeLookup => failure(
            exception::TYPE_ERROR,
            "prototype lookup is outside the closed-shape subset",
        ),
        _ => failure(
            exception::TYPE_ERROR,
            "JSON value is cyclic or too deeply nested",
        ),
    }
}

unsafe fn stringify(value: *mut u8, options: &StringifyOptions<'_>) -> *mut u8 {
    if value.is_null() {
        return ptr::null_mut();
    }
    let node = Node::Value(value);
    let size = match perry_json_codec::measure_tree_with_options(&RuntimeTree, node, options) {
        Ok(size) => size,
        Err(error) => return stringify_error(error),
    };
    let data = perry_alloc(
        size.try_into()
            .expect("measured JSON text fits wasm memory"),
    );
    // SAFETY: measured output is fresh and disjoint from the read-only JSON tree and controls.
    unsafe {
        let output = core::slice::from_raw_parts_mut(data, size);
        perry_json_codec::serialize_tree_with_options(&RuntimeTree, node, output, options)
            .expect("measured JSON serialization");
        string::perry_string_adopt(data, size as u32)
    }
}

/// # Safety
/// `value` is undefined or a live JSON tree, unchanged during serialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_stringify(value: *mut u8) -> *mut u8 {
    // SAFETY: default controls borrow no guest storage.
    unsafe { stringify(value, &StringifyOptions::default()) }
}

/// # Safety
/// Arguments are undefined or live JSON values; no guest code runs during serialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_stringify_options(
    value: *mut u8,
    replacer: *mut u8,
    space: *mut u8,
) -> *mut u8 {
    // SAFETY: controls borrow task strings and inspect live optional JSON references.
    unsafe {
        let properties = match property_list(replacer) {
            Ok(properties) => properties,
            Err(error) => return stringify_error(error),
        };
        let gap = gap(space);
        stringify(
            value,
            &StringifyOptions {
                gap,
                properties: properties.as_deref(),
            },
        )
    }
}
