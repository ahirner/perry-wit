//! A suspended depth-first writer. Callback calls occur only in generated code.
use super::*;
use crate::runtime::{Buffer, TYPE_ERROR, raise};
use perry_rt_sys::{json::MAX_DEPTH, layout::array};

#[repr(C)]
struct Frame {
    parent: *mut Frame,
    value: *mut u8,
    keys: *mut u8,
    member: *mut u8,
    index: u32,
    length: u32,
    array: bool,
    written: bool,
}

#[repr(C)]
struct Traversal {
    text: Buffer,
    frame: *mut Frame,
    keys: *mut u8,
    key: *mut u8,
    value: *mut u8,
    depth: u32,
    omitted: bool,
}

fn traversal(state: *mut u8) -> &'static mut Traversal {
    // SAFETY: only JsonTraversal MIR values reach these entrypoints.
    unsafe { &mut *state.cast::<Traversal>() }
}

fn key_at(keys: *mut u8, index: u32) -> *mut u8 {
    unsafe {
        node(word(
            node(word(keys, array::DATA as usize)),
            index as usize * 8,
        ))
    }
}

fn next_member(value: *mut u8, member: &mut *mut u8, key: &[u8]) -> *mut u8 {
    if unsafe { word(value, 0) } == ARRAY {
        let item = *member;
        if !item.is_null() {
            *member = node(unsafe { word(item, 4) });
        }
        item
    } else {
        property(value, key)
    }
}

fn property_list(value: *mut u8) -> *mut u8 {
    let Some(length) = view::array_length(value) else {
        return core::ptr::null_mut();
    };
    let (keys, slots) = crate::runtime::array_storage(length as usize);
    let mut count = 0;
    unsafe {
        let mut member = if word(value, 0) == ARRAY {
            node(word(value, 16))
        } else {
            core::ptr::null_mut()
        };
        for index in 0..length {
            let item = next_member(value, &mut member, string_bytes(index_key(index)));
            if item.is_null() || !matches!(word(item, 0), STRING | NUMBER | INT64 | UINT64) {
                continue;
            }
            let text = reflection::std_json_display(item);
            let bytes = crate::runtime::bytes(text);
            if slots[..count]
                .iter()
                .any(|&key| crate::runtime::bytes(node(key as u32)) == bytes)
            {
                continue;
            }
            slots[count] = text as u64;
            count += 1;
        }
        set_word(keys, array::LEN as usize, count as u32);
    }
    keys
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_replacer_begin(value: *mut u8, keys: *mut u8) -> *mut u8 {
    let state = perry_rt_sys::object!(Traversal; text, frame, keys, key, value);
    unsafe {
        state.write(Traversal {
            text: Buffer::new(),
            frame: core::ptr::null_mut(),
            keys: property_list(keys),
            key: from_static(perry_rt_sys::static_text!("")),
            value,
            depth: 0,
            omitted: false,
        })
    };
    state.cast()
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_replacer_key(state: *mut u8) -> *mut u8 {
    traversal(state).key
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_replacer_value(state: *mut u8) -> *mut u8 {
    let state = traversal(state);
    state.value = view::to_json(state.value);
    state.value
}

fn identity(value: *mut u8) -> *mut u8 {
    unsafe {
        if matches!(word(value, 0), VIEW | NATIVE | EXCEPTION) {
            node(word(value, 8))
        } else {
            value
        }
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_replacer_next(state: *mut u8, value: *mut u8) -> u32 {
    let state = traversal(state);
    unsafe {
        let absent = value.is_null() || word(value, 0) == UNDEFINED;
        let mut emit = !absent;
        if state.frame.is_null() {
            state.omitted = absent;
        } else {
            let frame = &mut *state.frame;
            emit |= frame.array;
            if emit {
                if frame.written {
                    state.text.push(b',');
                }
                frame.written = true;
                if !frame.array {
                    writer::quote(&mut state.text, crate::runtime::bytes(state.key));
                    state.text.push(b':');
                }
            }
        }
        if emit {
            if absent {
                state.text.extend_from_slice(b"null");
            } else if let Some(number) = json_number(value) {
                writer::append_number(&mut state.text, number);
            } else {
                match word(value, 0) {
                    NULL => state.text.extend_from_slice(b"null"),
                    BOOLEAN => state.text.extend_from_slice(if word(value, 8) == 0 {
                        b"false"
                    } else {
                        b"true"
                    }),
                    STRING => writer::quote(&mut state.text, string_bytes(value)),
                    BIGINT => {
                        raise(
                            TYPE_ERROR,
                            perry_rt_sys::static_text!("Do not know how to serialize a BigInt"),
                        );
                        return 0;
                    }
                    _ => {
                        let mut ancestor = state.frame;
                        while !ancestor.is_null() {
                            let frame = &*ancestor;
                            if identity(frame.value) == identity(value) {
                                raise(
                                    TYPE_ERROR,
                                    perry_rt_sys::static_text!(
                                        "Converting circular structure to JSON"
                                    ),
                                );
                                return 0;
                            }
                            ancestor = frame.parent;
                        }
                        if state.depth >= MAX_DEPTH {
                            raise(
                                TYPE_ERROR,
                                perry_rt_sys::static_text!(
                                    "JSON nesting exceeds the supported depth"
                                ),
                            );
                            return 0;
                        }
                        let length = view::array_length(value);
                        let keys = if length.is_some() {
                            core::ptr::null_mut()
                        } else if !state.keys.is_null() {
                            state.keys
                        } else {
                            std_json_keys(value)
                        };
                        let frame = perry_rt_sys::object!(Frame; parent, value, keys, member);
                        frame.write(Frame {
                            parent: state.frame,
                            value,
                            keys,
                            index: 0,
                            member: if word(value, 0) == ARRAY {
                                node(word(value, 16))
                            } else {
                                core::ptr::null_mut()
                            },
                            length: length.unwrap_or_else(|| word(keys, array::LEN as usize)),
                            array: length.is_some(),
                            written: false,
                        });
                        state.text.push(if length.is_some() { b'[' } else { b'{' });
                        state.frame = frame;
                        state.depth += 1;
                    }
                }
            }
        }
        while !state.frame.is_null() {
            let frame = &mut *state.frame;
            if frame.index < frame.length {
                state.key = if frame.array {
                    std_json_to_string(index_key(frame.index))
                } else {
                    key_at(frame.keys, frame.index)
                };
                frame.index += 1;
                state.value = next_member(
                    frame.value,
                    &mut frame.member,
                    crate::runtime::bytes(state.key),
                );
                return 1;
            }
            state.text.push(if frame.array { b']' } else { b'}' });
            state.frame = frame.parent;
            state.depth -= 1;
        }
        0
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_replacer_finish(state: *mut u8) -> *mut u8 {
    let state = traversal(state);
    if state.omitted {
        return core::ptr::null_mut();
    }
    let text = core::mem::take(&mut state.text);
    let bytes = text.as_slice();
    unsafe { crate::runtime::adopt(bytes.as_ptr().cast_mut(), bytes.len() as u32) }
}
