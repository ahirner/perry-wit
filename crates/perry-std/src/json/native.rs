//! Live native references and immutable snapshots.
use super::*;

/// A live native reference with its compiler-emitted layout descriptor.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_native(origin: *mut u8, descriptor: *mut u8) -> *mut u8 {
    let value = new_node(NATIVE);
    set_word(value, 8, address(origin));
    set_word(value, 12, address(descriptor));
    value
}
fn native_slot(bits: u64, descriptor: *mut u8) -> *mut u8 {
    match word(descriptor, 0) {
        1 => std_json_from_number(f64::from_bits(bits)),
        2 => std_json_from_scalar(BOOLEAN, bits as u32),
        3 => unsafe { std_json_from_string(node(bits as u32)) },
        4 | 5 => std_json_native(node(bits as u32), descriptor),
        6 => std_json_from_exception(node(bits as u32)),
        7 => {
            let inner = node(word(descriptor, 4));
            let (undefined, null) = match word(inner, 0) {
                1 => (crate::runtime::UNDEFINED_BITS, 0x7ff0_0000_0000_0002),
                2 => (2, 3),
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
        _ => unreachable!("native layout descriptor"),
    }
}
pub(super) fn native_property(value: *mut u8, name: &[u8]) -> *mut u8 {
    let (origin, descriptor) = (node(word(value, 8)), node(word(value, 12)));
    match word(descriptor, 0) {
        4 => {
            for index in 0..word(descriptor, 4) {
                let field = unsafe { descriptor.add(8 + index as usize * 12) };
                let key = unsafe { crate::runtime::bytes(node(word(field, 0))) };
                if key == name {
                    let slot = unsafe { *(origin.add(word(field, 8) as usize).cast::<u64>()) };
                    return native_slot(slot, node(word(field, 4)));
                }
            }
            core::ptr::null_mut()
        }
        5 => {
            let length = word(origin, 0);
            if name == b"length" {
                return std_json_from_number(f64::from(length));
            }
            if let Some(index) = array_index(name).filter(|&index| index < length) {
                let data = node(word(origin, crate::runtime::layout::array::DATA as usize));
                let slot = unsafe { *data.add(index as usize * 8).cast::<u64>() };
                native_slot(slot, node(word(descriptor, 4)))
            } else {
                core::ptr::null_mut()
            }
        }
        _ => unreachable!("native aggregate descriptor"),
    }
}
pub(super) fn native_snapshot(value: *mut u8) -> *mut u8 {
    let descriptor = node(word(value, 12));
    let result = new_node(if word(descriptor, 0) == 5 {
        ARRAY
    } else {
        OBJECT
    });
    let count = if word(descriptor, 0) == 5 {
        word(node(word(value, 8)), 0)
    } else {
        word(descriptor, 4)
    };
    for index in 0..count {
        if word(descriptor, 0) == 5 {
            let origin = node(word(value, 8));
            let data = node(word(origin, crate::runtime::layout::array::DATA as usize));
            let slot = unsafe { *data.add(index as usize * 8).cast::<u64>() };
            let item = native_slot(slot, node(word(descriptor, 4)));
            append(result, item, core::ptr::null_mut());
        } else {
            let name = node(word(descriptor, 8 + index as usize * 12));
            let key = unsafe { std_json_from_string(name) };
            let item = native_property(value, string_bytes(key));
            append(result, item, key);
        }
    }
    result
}
