//! Exception cause nodes, native reference reflection, and dynamic JSON equality/display.
use super::{
    address, append, array_index, json_number, new_node, node, set_word, std_json_from_i64,
    std_json_from_number, std_json_from_scalar, std_json_from_string, std_json_from_u64,
    std_json_to_string, string_bytes, word,
};
use crate::runtime::from_static;
use perry_rt_sys::{
    json::kind::*,
    layout::{descriptor, exception::*},
    number::Number,
};

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_exception(exception: *mut u8) -> *mut u8 {
    let value = new_node(EXCEPTION);
    unsafe { set_word(value, 8, address(exception)) };
    value
}

fn number_equal(left: Number, right: Number) -> bool {
    match (left, right) {
        (Number::Signed(i), Number::Unsigned(u)) | (Number::Unsigned(u), Number::Signed(i)) => {
            u64::try_from(i).ok() == Some(u)
        }
        (Number::Float(n), Number::Signed(i)) | (Number::Signed(i), Number::Float(n)) => {
            (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&n)
                && (n as i64) == i
                && (i as f64) == n
        }
        (Number::Float(n), Number::Unsigned(u)) | (Number::Unsigned(u), Number::Float(n)) => {
            (0.0..18_446_744_073_709_551_616.0).contains(&n) && (n as u64) == u && (u as f64) == n
        }
        _ => left == right,
    }
}

/// Strict equality preserves scalar values and identity of live cause references.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_strict_equal(left: *mut u8, right: *mut u8) -> u32 {
    if left.is_null() || right.is_null() {
        return u32::from(left == right);
    }
    if let (Some(left), Some(right)) = (json_number(left), json_number(right)) {
        return u32::from(number_equal(left, right));
    }
    unsafe {
        let (left_kind, right_kind) = (word(left, 0), word(right, 0));
        if matches!(left_kind, NATIVE | VIEW | EXCEPTION)
            && matches!(right_kind, NATIVE | VIEW | EXCEPTION)
        {
            return u32::from(word(left, 8) == word(right, 8));
        }
        if left_kind != right_kind {
            return 0;
        }
        u32::from(match left_kind {
            NULL => true,
            BOOLEAN => word(left, 8) == word(right, 8),
            STRING => string_bytes(left) == string_bytes(right),
            BIGINT => {
                crate::integers::std_bigint_compare(
                    node(word(left, 8)).cast(),
                    node(word(right, 8)).cast(),
                ) == 0
            }
            EXCEPTION | NATIVE | VIEW => word(left, 8) == word(right, 8),
            _ => left == right,
        })
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_origin_equal(value: *mut u8, origin: *mut u8) -> u32 {
    unsafe {
        u32::from(
            !value.is_null()
                && matches!(word(value, 0), EXCEPTION | NATIVE | VIEW)
                && word(value, 8) == address(origin),
        )
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_to_exception(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return perry_rt_sys::exception_value(TAG_UNDEFINED, 0);
    }
    let (tag, bits) = unsafe {
        match word(value, 0) {
            EXCEPTION => return node(word(value, 8)),
            NULL => (TAG_NULL, 0),
            BOOLEAN => (TAG_BOOLEAN, word(value, 8) as u64),
            NUMBER => {
                // SAFETY: this node holds a numeric payload.
                (TAG_NUMBER, *value.add(8).cast::<u64>())
            }
            STRING => (TAG_STRING, std_json_to_string(value) as usize as u64),
            _ => (TAG_JSON, value as usize as u64),
        }
    };
    perry_rt_sys::exception_value(tag, bits)
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_exception_equal(value: *mut u8, exception: *mut u8) -> u32 {
    unsafe {
        if word(exception, KIND as usize) != THROWN_VALUE {
            return std_json_origin_equal(value, exception);
        }
        let tag = word(exception, TAG as usize);
        // SAFETY: the tag identifies the exception's value slot.
        let bits = *exception.add(VALUE as usize).cast::<u64>();
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
            (TAG_STRING, STRING) => string_bytes(value) == crate::runtime::bytes(node(bits as u32)),
            (TAG_NUMBER, _) => json_number(value)
                .is_some_and(|number| number_equal(Number::Float(f64::from_bits(bits)), number)),
            _ => false,
        })
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_exception_is(value: *mut u8, expected: u32) -> u32 {
    unsafe {
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
}

/// ECMAScript ToString for supported dynamic primitives and cause references.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_display(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return from_static(perry_rt_sys::static_text!("undefined"));
    }
    if let Some(number) = json_number(value) {
        let (tag, bits) = number.into_numeric();
        return crate::number::std_number_to_string(tag, bits);
    }
    unsafe {
        match word(value, 0) {
            NULL => from_static(perry_rt_sys::static_text!("null")),
            BOOLEAN => from_static(if word(value, 8) == 0 {
                perry_rt_sys::static_text!("false")
            } else {
                perry_rt_sys::static_text!("true")
            }),
            STRING => std_json_to_string(value),
            BIGINT => crate::integers::std_bigint_to_string(node(word(value, 8)).cast()),
            _ => from_static(perry_rt_sys::static_text!("[object Object]")),
        }
    }
}

/// A live native reference with its compiler-emitted layout descriptor.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_native(origin: *mut u8, descriptor: *mut u8) -> *mut u8 {
    let value = new_node(NATIVE);
    unsafe {
        set_word(value, 8, address(origin));
        set_word(value, 12, address(descriptor));
    }
    value
}

fn native_slot(bits: u64, desc: *mut u8) -> *mut u8 {
    box_slot(bits, 0, desc, false)
}

pub(super) fn box_slot(bits: u64, tag: i32, desc: *mut u8, view: bool) -> *mut u8 {
    let flags = address(desc) & descriptor::ABSENCE_MASK;
    let desc = node(address(desc) & descriptor::SCHEMA_MASK as u32);
    unsafe {
        let kind = word(desc, 0);
        if kind == descriptor::JSON_NUMERIC {
            use perry_rt_sys::layout::numeric::Tag;
            return match Tag::try_from(tag) {
                Ok(Tag::Null) => super::std_json_from_scalar(NULL, 0),
                Ok(Tag::Undefined | Tag::Omitted) | Err(_) => core::ptr::null_mut(),
                Ok(tag) => super::numeric_node(Number::from_numeric(tag, bits)),
            };
        }
        if flags != 0 {
            let undefined = match kind {
                descriptor::NUMBER => descriptor::UNDEFINED_BITS,
                descriptor::BOOLEAN => descriptor::UNDEFINED_BOOL as u64,
                _ => 0,
            };
            let absence = bits.wrapping_sub(undefined);
            if absence == 1 && flags & descriptor::NULL_ALLOWED != 0 {
                return std_json_from_scalar(NULL, 0);
            }
            if absence <= 2 && absence != 1 && flags & descriptor::UNDEFINED_ALLOWED != 0 {
                return core::ptr::null_mut();
            }
        }
        match kind {
            descriptor::NUMBER => std_json_from_number(f64::from_bits(bits)),
            descriptor::BOOLEAN => std_json_from_scalar(BOOLEAN, bits as u32),
            descriptor::SIGNED_INT | descriptor::UNSIGNED_INT => {
                let shift = 64 - word(desc, 4);
                if kind == descriptor::SIGNED_INT {
                    std_json_from_i64(((bits << shift) as i64) >> shift)
                } else {
                    std_json_from_u64((bits << shift) >> shift)
                }
            }
            descriptor::STRING => std_json_from_string(node(bits as u32)),
            descriptor::RECORD
            | descriptor::JSON_TUPLE
            | descriptor::ARRAY
            | descriptor::JSON_DICTIONARY
            | descriptor::JSON_VARIANT
            | descriptor::JSON_DATE => {
                let value = std_json_native(node(bits as u32), desc);
                if view {
                    set_word(value, 0, VIEW);
                }
                value
            }
            descriptor::JSON_VALUE => node(bits as u32),
            descriptor::JSON_BIGINT => {
                let value = new_node(BIGINT);
                set_word(value, 8, bits as u32);
                value
            }
            descriptor::JSON_NUMBER_STRING => {
                let value = node(bits as u32);
                if word(value, 0) == word(desc, 8) {
                    std_json_from_number(*value.add(8).cast::<f64>())
                } else {
                    std_json_from_string(node(word(value, 8)))
                }
            }
            descriptor::EXCEPTION => std_json_from_exception(node(bits as u32)),
            _ => core::ptr::null_mut(),
        }
    }
}

fn native_field(origin: *mut u8, desc: *mut u8, index: u32) -> (*mut u8, *mut u8) {
    unsafe {
        let field = desc.add((descriptor::FIELDS + index * descriptor::FIELD_SIZE) as usize);
        let slot = *origin
            .add(word(field, descriptor::FIELD_OFFSET as usize) as usize)
            .cast::<u64>();
        (
            node(word(field, 0)),
            native_slot(slot, node(word(field, descriptor::FIELD_SCHEMA as usize))),
        )
    }
}

fn native_element(origin: *mut u8, desc: *mut u8, index: u32) -> *mut u8 {
    unsafe {
        let data = node(word(origin, crate::runtime::layout::array::DATA as usize));
        let slot = *data.add(index as usize * 8).cast::<u64>();
        native_slot(slot, node(word(desc, 4)))
    }
}

pub(super) fn native_property(value: *mut u8, name: &[u8]) -> *mut u8 {
    unsafe {
        let (origin, desc) = (node(word(value, 8)), node(word(value, 12)));
        match word(desc, 0) {
            descriptor::RECORD | descriptor::JSON_TUPLE => {
                for index in 0..word(desc, 4) {
                    let (key, item) = native_field(origin, desc, index);
                    if crate::runtime::bytes(key) == name {
                        return item;
                    }
                }
                core::ptr::null_mut()
            }
            descriptor::ARRAY | descriptor::JSON_DICTIONARY => {
                let length = word(origin, 0);
                if name == b"length" {
                    return std_json_from_number(f64::from(length));
                }
                array_index(name)
                    .filter(|&index| index < length)
                    .map_or(core::ptr::null_mut(), |index| {
                        native_element(origin, desc, index)
                    })
            }
            _ => core::ptr::null_mut(),
        }
    }
}

pub(super) fn native_snapshot(value: *mut u8) -> *mut u8 {
    unsafe {
        let (origin, desc) = (node(word(value, 8)), node(word(value, 12)));
        let is_array = matches!(
            word(desc, 0),
            descriptor::ARRAY | descriptor::JSON_DICTIONARY
        );
        let result = new_node(if is_array { ARRAY } else { OBJECT });
        let count = if is_array {
            word(origin, 0)
        } else {
            word(desc, 4)
        };
        for index in 0..count {
            if is_array {
                append(
                    result,
                    native_element(origin, desc, index),
                    core::ptr::null_mut(),
                );
            } else {
                let (key, item) = native_field(origin, desc, index);
                append(result, item, std_json_from_string(key));
            }
        }
        result
    }
}
