//! Lazy JSON views over native slots: values are read when the replacer visits them.
use super::*;
use perry_rt_sys::json::METADATA;
use perry_rt_sys::layout::{array, descriptor as d, record, variant};

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_box(bits: u64, tag: i32, schema: *mut u8) -> *mut u8 {
    reflection::box_slot(bits, tag, schema, true)
}

fn slot(origin: *mut u8, offset: u32, schema: *mut u8) -> *mut u8 {
    let (bits, tag) = unsafe {
        let at = origin.add(offset as usize);
        let numeric = word(node(address(schema) & d::SCHEMA_MASK as u32), 0) == d::JSON_NUMERIC;
        if numeric {
            (*at.add(8).cast::<u64>(), word(at, 0) as i32)
        } else {
            (*at.cast::<u64>(), 0)
        }
    };
    std_json_box(bits, tag, schema)
}

fn field(origin: *mut u8, schema: *mut u8, index: u32) -> *mut u8 {
    let (offset, field_schema) = unsafe {
        let entry = schema.add((d::FIELDS + index * d::FIELD_SIZE) as usize);
        (
            word(entry, d::FIELD_OFFSET as usize),
            node(word(entry, d::FIELD_SCHEMA as usize)),
        )
    };
    slot(origin, offset, field_schema)
}

pub(super) fn array_length(value: *mut u8) -> Option<u32> {
    if value.is_null() {
        return None;
    }
    unsafe {
        match word(value, 0) {
            ARRAY => Some(word(value, MEMBER_COUNT)),
            VIEW => {
                let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
                match word(schema, 0) {
                    d::ARRAY => Some(word(origin, array::LEN as usize)),
                    d::JSON_TUPLE => {
                        let graph = node(word(origin, record::JSON as usize));
                        Some(word(schema, 4).max(if graph.is_null() {
                            0
                        } else {
                            word(graph, METADATA) + word(graph, MEMBER_COUNT)
                        }))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }
}

fn variant_case(origin: *mut u8, schema: *mut u8) -> Option<*mut u8> {
    unsafe {
        let tag = crate::runtime::bytes(node(word(origin, variant::TAG as usize)));
        (0..word(schema, 4)).find_map(|index| {
            let case = schema.add((d::FIELDS + index * d::FIELD_SIZE) as usize);
            (crate::runtime::bytes(node(word(case, 0))) == tag).then_some(case)
        })
    }
}

fn variant_graph(origin: *mut u8, schema: *mut u8) -> *mut u8 {
    unsafe {
        if word(
            schema,
            (d::FIELDS + word(schema, 4) * d::FIELD_SIZE + 8) as usize,
        ) == 0
        {
            core::ptr::null_mut()
        } else {
            node(word(origin, variant::JSON as usize))
        }
    }
}

pub(super) fn property(value: *mut u8, name: &[u8]) -> *mut u8 {
    unsafe {
        let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
        let count = word(schema, 4);
        match word(schema, 0) {
            d::RECORD => {
                for index in 0..count {
                    let entry = schema.add((d::FIELDS + index * d::FIELD_SIZE) as usize);
                    if crate::runtime::bytes(node(word(entry, 0))) == name {
                        return field(origin, schema, index);
                    }
                }
                let graph = node(word(origin, record::JSON as usize));
                if graph.is_null() {
                    core::ptr::null_mut()
                } else {
                    object_member(graph, name)
                }
            }
            d::ARRAY | d::JSON_TUPLE => {
                if name == b"length" {
                    return std_json_from_number(f64::from(array_length(value).unwrap_or(0)));
                }
                let Some(index) = array_index(name) else {
                    return core::ptr::null_mut();
                };
                if word(schema, 0) == d::JSON_TUPLE {
                    if index < count {
                        field(origin, schema, index)
                    } else {
                        let graph = node(word(origin, record::JSON as usize));
                        if graph.is_null() {
                            core::ptr::null_mut()
                        } else {
                            index
                                .checked_sub(word(graph, METADATA))
                                .map_or(core::ptr::null_mut(), |index| array_element(graph, index))
                        }
                    }
                } else if index < word(origin, array::LEN as usize) {
                    let stride = array::Elements::decode(word(schema, 8)).stride();
                    slot(
                        node(word(origin, array::DATA as usize)),
                        index * stride,
                        node(count),
                    )
                } else {
                    core::ptr::null_mut()
                }
            }
            d::JSON_DICTIONARY => {
                let entry_schema = node(count);
                let data = node(word(origin, array::DATA as usize));
                for index in (0..word(origin, array::LEN as usize)).rev() {
                    let entry = node(word(data, index as usize * 8));
                    let key = field(entry, entry_schema, 0);
                    let text = reflection::std_json_display(key);
                    if crate::runtime::bytes(text) == name {
                        return field(entry, entry_schema, 1);
                    }
                }
                core::ptr::null_mut()
            }
            d::JSON_VARIANT => {
                let Some(case) = variant_case(origin, schema) else {
                    return core::ptr::null_mut();
                };
                match name {
                    b"tag" => std_json_from_string(node(word(case, 0))),
                    b"val" if word(case, 4) != 0 => {
                        slot(origin, variant::VALUE, node(word(case, 4)))
                    }
                    _ => {
                        let graph = variant_graph(origin, schema);
                        if graph.is_null() {
                            core::ptr::null_mut()
                        } else {
                            object_member(graph, name)
                        }
                    }
                }
            }
            _ => core::ptr::null_mut(),
        }
    }
}

pub(super) fn keys(value: *mut u8) -> *mut u8 {
    if let Some(length) = array_length(value) {
        return super::index_keys(length);
    }
    unsafe {
        let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
        let count = word(schema, 4);
        let graph = match word(schema, 0) {
            d::RECORD => {
                let table = node(word(
                    schema,
                    (d::FIELDS + count * d::FIELD_SIZE + 8) as usize,
                ));
                super::order::order_graph(origin, table.cast(), count)
            }
            d::JSON_DICTIONARY => {
                let graph = new_node(OBJECT);
                let data = node(word(origin, array::DATA as usize));
                for index in 0..word(origin, array::LEN as usize) {
                    let entry = node(word(data, index as usize * 8));
                    let key = reflection::std_json_display(field(entry, node(count), 0));
                    append(graph, core::ptr::null_mut(), std_json_from_string(key));
                }
                graph
            }
            d::JSON_VARIANT => {
                let Some(case) = variant_case(origin, schema) else {
                    return crate::runtime::array(&[]);
                };
                let payload = word(case, 4) != 0;
                let graph = variant_graph(origin, schema);
                let graph = if graph.is_null() {
                    graph
                } else {
                    canonical(graph)
                };
                let retained = if graph.is_null() {
                    0
                } else {
                    word(graph, MEMBER_COUNT) as usize
                };
                let (keys, slots) =
                    crate::runtime::array_storage(retained + 1 + usize::from(payload));
                let mut member = if graph.is_null() { 0 } else { word(graph, 16) };
                let mut length = 0;
                while member != 0 && array_index(key_bytes(member)).is_some() {
                    slots[length] = std_json_to_string(node(word(node(member), 24))) as u64;
                    length += 1;
                    member = word(node(member), 4);
                }
                slots[length] = from_static("tag") as u64;
                length += 1;
                if payload {
                    slots[length] = from_static("val") as u64;
                    length += 1;
                }
                while member != 0 {
                    let key = node(word(node(member), 24));
                    let name = string_bytes(key);
                    if name != b"tag" && !(payload && name == b"val") {
                        slots[length] = std_json_to_string(key) as u64;
                        length += 1;
                    }
                    member = word(node(member), 4);
                }
                set_word(keys, array::LEN as usize, length as u32);
                return keys;
            }
            _ => return crate::runtime::array(&[]),
        };
        let graph = canonical(graph);
        let (keys, slots) = crate::runtime::array_storage(word(graph, MEMBER_COUNT) as usize);
        let mut member = word(graph, 16);
        for slot in slots {
            *slot = std_json_to_string(node(word(node(member), 24))) as u64;
            member = word(node(member), 4);
        }
        keys
    }
}

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "__perry_helper")]
unsafe extern "C" {
    fn time_date_iso(ms: f64, out: *mut u8, capacity: u32) -> u64;
}

pub(super) fn to_json(value: *mut u8) -> *mut u8 {
    unsafe {
        if !value.is_null()
            && word(value, 0) == VIEW
            && word(node(word(value, 12)), 0) == d::JSON_DATE
        {
            #[cfg(target_arch = "wasm32")]
            {
                let mut text = [0; d::WRITE_DATE_CAPACITY as usize];
                let ms = *node(word(value, 8)).cast::<f64>();
                let result = time_date_iso(ms, text.as_mut_ptr(), text.len() as u32);
                return if result >> 32 == 0 {
                    std_json_from_string(crate::runtime::from_bytes(
                        &text[..result as u32 as usize],
                    ))
                } else {
                    std_json_from_scalar(NULL, 0)
                };
            }
        }
        value
    }
}

/// Materialize just one level for consumers of an immutable graph.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_snapshot(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return value;
    }
    unsafe {
        if word(value, 0) != VIEW {
            return value;
        }
        let length = array_length(value);
        let graph = new_node(if length.is_some() { ARRAY } else { OBJECT });
        let names = if length.is_some() {
            core::ptr::null_mut()
        } else {
            keys(value)
        };
        let count = length.unwrap_or_else(|| word(names, array::LEN as usize));
        for index in 0..count {
            let key = if length.is_some() {
                index_key(index)
            } else {
                let data = node(word(names, array::DATA as usize));
                std_json_from_string(node(word(data, index as usize * 8)))
            };
            append(
                graph,
                property(value, string_bytes(key)),
                if length.is_some() {
                    core::ptr::null_mut()
                } else {
                    key
                },
            );
        }
        graph
    }
}
