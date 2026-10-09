//! Compact JSON serialization with bounded traversal.
use super::*;

pub(super) fn stringify_error(error: Error) -> *mut u8 {
    match error {
        Error::Capacity => failure(exception::RANGE_ERROR, "JSON text is too large"),
        _ => failure(
            exception::TYPE_ERROR,
            "JSON value is cyclic or too deeply nested",
        ),
    }
}

/// # Safety
/// `value` is undefined or a live JSON tree, unchanged during serialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn perry_json_stringify(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return ptr::null_mut();
    }
    let node = Node::Value(value);
    let size = match perry_json_codec::measure_tree(&RuntimeTree, node) {
        Ok(size) => size,
        Err(error) => return stringify_error(error),
    };
    let data = perry_alloc(
        size.try_into()
            .expect("measured JSON text fits wasm memory"),
    );
    // SAFETY: measured output is fresh and disjoint from the read-only JSON tree tree.
    unsafe {
        let output = core::slice::from_raw_parts_mut(data, size);
        perry_json_codec::serialize_tree(&RuntimeTree, node, output)
            .expect("measured JSON serialization");
        string::perry_string_adopt(data, size as u32)
    }
}
