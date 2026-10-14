//! Exception cause nodes, native reference reflection, and dynamic JSON equality/display.
use super::{
    address, append, array_index, new_node, node, set_word, std_json_from_number,
    std_json_from_scalar, std_json_from_string, std_json_to_string, string_bytes, word,
};
use crate::runtime::from_static;
use perry_abi::{
    json::kind::*,
    layout::{descriptor, exception::*},
};

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_exception(exception: *mut u8) -> *mut u8 {
    let value = new_node(EXCEPTION);
    set_word(value, 8, address(exception));
    value
}

/// Strict equality preserves scalar values and identity of live cause references.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_strict_equal(left: *mut u8, right: *mut u8) -> u32 {
    if left.is_null() || right.is_null() {
        return u32::from(left == right);
    }
    let kind = word(left, 0);
    if kind != word(right, 0) {
        return 0;
    }
    u32::from(match kind {
        NULL => true,
        BOOLEAN => word(left, 8) == word(right, 8),
        STRING => string_bytes(left) == string_bytes(right),
        NUMBER => {
            // SAFETY: both nodes hold f64 payloads.
            unsafe { *left.add(8).cast::<f64>() == *right.add(8).cast::<f64>() }
        }
        EXCEPTION | NATIVE => word(left, 8) == word(right, 8),
        _ => left == right,
    })
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_origin_equal(value: *mut u8, origin: *mut u8) -> u32 {
    u32::from(
        !value.is_null()
            && matches!(word(value, 0), EXCEPTION | NATIVE)
            && word(value, 8) == address(origin),
    )
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_to_exception(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return perry_abi::exception_value(TAG_UNDEFINED, 0);
    }
    let (tag, bits) = match word(value, 0) {
        EXCEPTION => return node(word(value, 8)),
        NULL => (TAG_NULL, 0),
        BOOLEAN => (TAG_BOOLEAN, word(value, 8) as u64),
        NUMBER => {
            // SAFETY: this node holds a numeric payload.
            (TAG_NUMBER, unsafe { *value.add(8).cast::<u64>() })
        }
        STRING => (TAG_STRING, std_json_to_string(value) as usize as u64),
        _ => (TAG_JSON, value as usize as u64),
    };
    perry_abi::exception_value(tag, bits)
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_exception_equal(value: *mut u8, exception: *mut u8) -> u32 {
    if word(exception, KIND as usize) != THROWN_VALUE {
        return std_json_origin_equal(value, exception);
    }
    let tag = word(exception, TAG as usize);
    // SAFETY: the tag identifies the exception's value slot.
    let bits = unsafe { *exception.add(VALUE as usize).cast::<u64>() };
    if tag == TAG_UNDEFINED {
        return u32::from(value.is_null());
    }
    if tag == TAG_JSON {
        return std_json_strict_equal(value, node(bits as u32));
    }
    if value.is_null() {
        return 0;
    }
    u32::from(match (tag, word(value, 0)) {
        (TAG_NULL, NULL) => true,
        (TAG_BOOLEAN, BOOLEAN) => bits == word(value, 8) as u64,
        (TAG_STRING, STRING) => {
            string_bytes(value) == unsafe { crate::runtime::bytes(node(bits as u32)) }
        }
        (TAG_NUMBER, NUMBER) => f64::from_bits(bits) == unsafe { *value.add(8).cast::<f64>() },
        _ => false,
    })
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_exception_is(value: *mut u8, expected: u32) -> u32 {
    if value.is_null() || word(value, 0) != EXCEPTION {
        return 0;
    }
    let kind = word(node(word(value, 8)), 0);
    u32::from(if expected == 0 {
        matches!(
            kind,
            TYPE_ERROR | RANGE_ERROR | ERROR | REFERENCE_ERROR | SYNTAX_ERROR
        )
    } else {
        kind == expected
    })
}

/// ECMAScript ToString for supported dynamic primitives and cause references.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_display(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return from_static("undefined");
    }
    match word(value, 0) {
        NULL => from_static("null"),
        BOOLEAN => from_static(if word(value, 8) == 0 { "false" } else { "true" }),
        STRING => std_json_to_string(value),
        NUMBER => {
            // SAFETY: number nodes hold an f64 at 8.
            let number = unsafe { *(value.add(8) as *const f64) };
            if number.is_finite() {
                let mut digits = [0; 40];
                let len = crate::runtime::number_format(number, &mut digits);
                crate::runtime::from_bytes(&digits[..len])
            } else {
                from_static("null")
            }
        }
        _ => from_static("[object Object]"),
    }
}

/// A live native reference with its compiler-emitted layout descriptor.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_native(origin: *mut u8, descriptor: *mut u8) -> *mut u8 {
    let value = new_node(NATIVE);
    set_word(value, 8, address(origin));
    set_word(value, 12, address(descriptor));
    value
}

fn native_slot(bits: u64, desc: *mut u8) -> *mut u8 {
    match word(desc, 0) {
        descriptor::NUMBER => std_json_from_number(f64::from_bits(bits)),
        descriptor::BOOLEAN => std_json_from_scalar(BOOLEAN, bits as u32),
        descriptor::STRING => unsafe { std_json_from_string(node(bits as u32)) },
        descriptor::RECORD | descriptor::ARRAY => std_json_native(node(bits as u32), desc),
        descriptor::EXCEPTION => std_json_from_exception(node(bits as u32)),
        descriptor::OPTIONAL => {
            let inner = node(word(desc, 4));
            let (undefined, null) = match word(inner, 0) {
                descriptor::NUMBER => {
                    (crate::runtime::UNDEFINED_BITS, descriptor::NULL_NUMBER_BITS)
                }
                descriptor::BOOLEAN => (2, 3),
                _ => (0, 1),
            };
            if bits == undefined {
                core::ptr::null_mut()
            } else if bits == null {
                std_json_from_scalar(NULL, 0)
            } else {
                native_slot(bits, inner)
            }
        }
        _ => core::ptr::null_mut(),
    }
}

pub(super) fn native_property(value: *mut u8, name: &[u8]) -> *mut u8 {
    let (origin, desc) = (node(word(value, 8)), node(word(value, 12)));
    match word(desc, 0) {
        descriptor::RECORD => {
            for index in 0..word(desc, 4) {
                let field = unsafe { desc.add(8 + index as usize * 12) };
                let key = unsafe { crate::runtime::bytes(node(word(field, 0))) };
                if key == name {
                    let slot = unsafe { *(origin.add(word(field, 8) as usize).cast::<u64>()) };
                    return native_slot(slot, node(word(field, 4)));
                }
            }
            core::ptr::null_mut()
        }
        descriptor::ARRAY => {
            let length = word(origin, 0);
            if name == b"length" {
                return std_json_from_number(f64::from(length));
            }
            if let Some(index) = array_index(name).filter(|&index| index < length) {
                let data = node(word(origin, crate::runtime::layout::array::DATA as usize));
                let slot = unsafe { *data.add(index as usize * 8).cast::<u64>() };
                native_slot(slot, node(word(desc, 4)))
            } else {
                core::ptr::null_mut()
            }
        }
        _ => core::ptr::null_mut(),
    }
}

pub(super) fn native_snapshot(value: *mut u8) -> *mut u8 {
    let (origin, desc) = (node(word(value, 8)), node(word(value, 12)));
    let is_array = word(desc, 0) == descriptor::ARRAY;
    let result = new_node(if is_array { ARRAY } else { OBJECT });
    let count = if is_array {
        word(origin, 0)
    } else {
        word(desc, 4)
    };
    for index in 0..count {
        if is_array {
            let data = node(word(origin, crate::runtime::layout::array::DATA as usize));
            let slot = unsafe { *data.add(index as usize * 8).cast::<u64>() };
            let item = native_slot(slot, node(word(desc, 4)));
            append(result, item, core::ptr::null_mut());
        } else {
            let field = unsafe { desc.add(8 + index as usize * 12) };
            let key = unsafe { std_json_from_string(node(word(field, 0))) };
            let slot = unsafe { *origin.add(word(field, 8) as usize).cast::<u64>() };
            let item = native_slot(slot, node(word(field, 4)));
            append(result, item, key);
        }
    }
    result
}
