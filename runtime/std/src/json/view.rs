//! Lazy JSON views over native slots: values are read when the replacer visits them.
use super::*;
use perry_rt_sys::json::METADATA;
use perry_rt_sys::layout::{array, descriptor as d, record, variant};

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_box(bits: u64, tag: i32, schema: *mut u8) -> *mut u8 {
    reflection::box_slot(bits, tag, schema, true)
}

fn read_slot(origin: *mut u8, offset: u32, schema: *mut u8, view: bool) -> *mut u8 {
    let (bits, tag) = unsafe {
        let at = origin.add(offset as usize);
        let numeric = word(node(address(schema) & d::SCHEMA_MASK as u32), 0) == d::JSON_NUMERIC;
        if numeric {
            (*at.add(8).cast::<u64>(), word(at, 0) as i32)
        } else {
            (*at.cast::<u64>(), 0)
        }
    };
    reflection::box_slot(bits, tag, schema, view)
}

unsafe fn field_descriptor(schema: *mut u8, index: u32) -> *mut u8 {
    unsafe { schema.add((d::FIELDS + index * d::FIELD_SIZE) as usize) }
}

unsafe fn field_table(schema: *mut u8, count: u32) -> *const u32 {
    unsafe {
        node(word(
            schema,
            (d::FIELDS + count * d::FIELD_SIZE + 8) as usize,
        ))
        .cast()
    }
}

unsafe fn field_key(schema: *mut u8, index: u32) -> *mut u8 {
    unsafe { node(word(field_descriptor(schema, index), 0)) }
}

fn field(origin: *mut u8, schema: *mut u8, index: u32, view: bool) -> *mut u8 {
    let (offset, field_schema) = unsafe {
        let entry = field_descriptor(schema, index);
        (
            word(entry, d::FIELD_OFFSET as usize),
            node(word(entry, d::FIELD_SCHEMA as usize)),
        )
    };
    read_slot(origin, offset, field_schema, view)
}

fn array_slot(origin: *mut u8, schema: *mut u8, index: u32, view: bool) -> Option<*mut u8> {
    array_present(origin, index).then(|| unsafe {
        let stride = array::Elements::decode(word(schema, 8)).stride();
        read_slot(
            node(word(origin, array::DATA as usize)),
            index * stride,
            node(word(schema, 4)),
            view,
        )
    })
}

fn tuple_slot(origin: *mut u8, schema: *mut u8, index: u32) -> *mut u8 {
    unsafe {
        if index < word(schema, 4) {
            return field(origin, schema, index, true);
        }
        let graph = node(word(origin, record::JSON as usize));
        if graph.is_null() {
            return core::ptr::null_mut();
        }
        index
            .checked_sub(word(graph, METADATA))
            .map_or(core::ptr::null_mut(), |i| array_element(graph, i))
    }
}

fn tuple_length(origin: *mut u8, schema: *mut u8) -> u32 {
    unsafe {
        let declared = word(schema, 4);
        let graph = node(word(origin, record::JSON as usize));
        if graph.is_null() {
            declared
        } else {
            declared.max(word(graph, METADATA) + word(graph, MEMBER_COUNT))
        }
    }
}

pub(super) fn array_length(value: *mut u8) -> Option<u32> {
    if value.is_null() {
        return None;
    }
    unsafe {
        match word(value, 0) {
            ARRAY => Some(word(value, MEMBER_COUNT)),
            VIEW | NATIVE => {
                let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
                match word(schema, 0) {
                    d::ARRAY => Some(word(origin, array::LEN as usize)),
                    d::JSON_TUPLE if word(value, 0) == VIEW => Some(tuple_length(origin, schema)),
                    d::JSON_TUPLE => Some(word(schema, 4)),
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
        let view = word(value, 0) == VIEW;
        match (word(schema, 0), view) {
            (d::RECORD, _) | (d::JSON_TUPLE, false) => {
                let table = field_table(schema, count);
                let index = super::order::lookup_field(table, count, name);
                if index < count {
                    return field(origin, schema, index, view);
                }
                if view {
                    object_member(node(word(origin, record::JSON as usize)), name)
                } else {
                    core::ptr::null_mut()
                }
            }
            (d::ARRAY, _) | (d::JSON_DICTIONARY, false) => {
                let length = word(origin, array::LEN as usize);
                if name == b"length" {
                    return std_json_from_number(f64::from(length));
                }
                array_index(name)
                    .filter(|&i| i < length)
                    .and_then(|i| array_slot(origin, schema, i, view))
                    .unwrap_or_default()
            }
            (d::JSON_TUPLE, true) => {
                if name == b"length" {
                    return std_json_from_number(f64::from(tuple_length(origin, schema)));
                }
                array_index(name).map_or(core::ptr::null_mut(), |i| tuple_slot(origin, schema, i))
            }
            (d::JSON_DICTIONARY, true) => {
                let entry_schema = node(count);
                let data = node(word(origin, array::DATA as usize));
                for index in (0..word(origin, array::LEN as usize)).rev() {
                    let entry = node(word(data, index as usize * 8));
                    let key = field(entry, entry_schema, 0, true);
                    let text = reflection::std_json_display(key);
                    if crate::runtime::bytes(text) == name {
                        return field(entry, entry_schema, 1, true);
                    }
                }
                core::ptr::null_mut()
            }
            (d::JSON_VARIANT, true) => {
                let Some(case) = variant_case(origin, schema) else {
                    return core::ptr::null_mut();
                };
                match name {
                    b"tag" => std_json_from_string(node(word(case, 0))),
                    b"val" if word(case, 4) != 0 => {
                        read_slot(origin, variant::VALUE, node(word(case, 4)), true)
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
        unsafe {
            if matches!(word(value, 0), VIEW | NATIVE) {
                let origin = node(word(value, 8));
                let schema = node(word(value, 12));
                if word(schema, 0) == d::ARRAY && word(origin, array::HOLES as usize) != 0 {
                    return crate::runtime::array(
                        (0..length)
                            .filter(|&index| array_present(origin, index))
                            .map(index_text),
                    );
                }
            }
        }
        return super::index_keys(length);
    }
    unsafe {
        let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
        let count = word(schema, 4);
        let graph = match word(schema, 0) {
            d::RECORD => super::order::order_graph(origin, field_table(schema, count), count),
            d::JSON_DICTIONARY => {
                let graph = new_node(OBJECT);
                let data = node(word(origin, array::DATA as usize));
                for index in 0..word(origin, array::LEN as usize) {
                    let entry = node(word(data, index as usize * 8));
                    let key = reflection::std_json_display(field(entry, node(count), 0, true));
                    append(graph, core::ptr::null_mut(), std_json_from_string(key));
                }
                graph
            }
            d::JSON_VARIANT => {
                let Some(case) = variant_case(origin, schema) else {
                    return crate::runtime::array([]);
                };
                let payload = word(case, 4) != 0;
                let graph = canonical(variant_graph(origin, schema));
                let mut members = children(graph).peekable();
                let key = |member| node(word(node(member), 24));
                let capacity = if graph.is_null() {
                    0
                } else {
                    word(graph, MEMBER_COUNT)
                };
                let mut keys = crate::runtime::ReferenceArray::with_capacity(
                    capacity.saturating_add(1 + u32::from(payload)),
                );
                while let Some(member) =
                    members.next_if(|&member| array_index(key_bytes(member)).is_some())
                {
                    keys.push(std_json_to_string(key(member)));
                }
                keys.push(from_static(perry_rt_sys::static_text!("tag")));
                if payload {
                    keys.push(from_static(perry_rt_sys::static_text!("val")));
                }
                for member in members {
                    let key = key(member);
                    let name = string_bytes(key);
                    if name != b"tag" && !(payload && name == b"val") {
                        keys.push(std_json_to_string(key));
                    }
                }
                return keys.finish();
            }
            _ => return crate::runtime::array([]),
        };
        let graph = canonical(graph);
        crate::runtime::array(
            children(graph).map(|member| std_json_to_string(node(word(node(member), 24)))),
        )
    }
}

fn array_present(origin: *mut u8, index: u32) -> bool {
    unsafe { !array::is_hole(node(word(origin, array::HOLES as usize)), index) }
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

fn snapshot(kind: u32, members: impl Iterator<Item = (*mut u8, *mut u8)>) -> *mut u8 {
    let graph = new_node(kind);
    for (item, key) in members {
        append(graph, item, key);
    }
    graph
}

/// Materialize one level, preserving holes, field presence and property order.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_snapshot(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return value;
    }
    unsafe {
        let view = match word(value, 0) {
            VIEW => true,
            NATIVE => false,
            _ => return value,
        };
        let (origin, schema) = (node(word(value, 8)), node(word(value, 12)));
        let kind = word(schema, 0);
        match (kind, view) {
            (d::ARRAY, _) | (d::JSON_DICTIONARY, false) => {
                let elements = (0..word(origin, array::LEN as usize)).map(|i| {
                    let item =
                        array_slot(origin, schema, i, view).unwrap_or_else(|| new_node(ABSENT));
                    (item, core::ptr::null_mut())
                });
                snapshot(ARRAY, elements)
            }
            (d::JSON_TUPLE, true) => {
                let elements = (0..tuple_length(origin, schema))
                    .map(|i| (tuple_slot(origin, schema, i), core::ptr::null_mut()));
                snapshot(ARRAY, elements)
            }
            (_, false) => {
                let count = word(schema, 4);
                let fields = (0..count).map(|i| {
                    let name = field_key(schema, i);
                    let mut item = field(origin, schema, i, false);
                    if item.is_null()
                        && kind == d::RECORD
                        && !super::order::field_present(
                            origin,
                            count,
                            i,
                            crate::runtime::bytes(name),
                        )
                    {
                        item = new_node(ABSENT);
                    }
                    (item, std_json_from_string(name))
                });
                snapshot(OBJECT, fields)
            }
            (_, true) => {
                let names = keys(value);
                let data = node(word(names, array::DATA as usize));
                let fields = (0..word(names, array::LEN as usize)).map(|i| {
                    let name = node(word(data, i as usize * 8));
                    (
                        property(value, crate::runtime::bytes(name)),
                        std_json_from_string(name),
                    )
                });
                snapshot(OBJECT, fields)
            }
        }
    }
}
