//! Record headers carry compact field order or a graph retaining interleaved extra keys.
use super::{
    address, append, canonical, exclude_key, key_bytes, new_node, node, set_word, std_json_assign,
    string_node, word,
};
use perry_abi::json::{
    FIRST, NEXT, PACKED_FIELDS, append_field, field_index, field_name, kind::OBJECT, remove_field,
};
use perry_abi::layout::record;

/// Find the native field overlaid on this graph member; `count` denotes an extra key.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_record_index(
    member: u32,
    table: *const u32,
    count: u32,
) -> u32 {
    let key = key_bytes(member);
    unsafe {
        field_index(table, count, |name| {
            key.len().cmp(&name.len()).then_with(|| key.cmp(name))
        })
    }
}

/// Start a canonical object traversal, retaining the graph's first-insertion positions.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_members(graph: *mut u8) -> u32 {
    if graph.is_null() {
        0
    } else {
        word(canonical(graph), FIRST)
    }
}

/// A fully decoded small record can discard its graph when every key is declared.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_record_retain(
    native: *mut u8,
    graph: *mut u8,
    table: *const u32,
    count: u32,
) {
    if graph.is_null() {
        set_word(native, record::ORDER as usize, 0);
        set_word(native, record::JSON as usize, 0);
        return;
    }
    let mut order = 0;
    if count as usize <= PACKED_FIELDS {
        let mut member = word(graph, FIRST);
        while member != 0 {
            let index = unsafe { std_json_record_index(member, table, count) };
            if index == count {
                set_word(native, record::JSON as usize, address(graph));
                return;
            }
            order = append_field(order, index);
            member = word(node(member), NEXT);
        }
        set_word(native, record::ORDER as usize, order);
        set_word(native, record::JSON as usize, 0);
    } else {
        set_word(native, record::JSON as usize, address(graph));
    }
}

/// Materialize keys only when a record outgrows compact order or carries extra members.
unsafe fn order_graph(native: *mut u8, table: *const u32, count: u32) -> *mut u8 {
    let graph = node(word(native, record::JSON as usize));
    if !graph.is_null() {
        return graph;
    }
    let graph = new_node(OBJECT);
    let mut order = word(native, record::ORDER as usize);
    let add = |index| {
        let key = unsafe { crate::runtime::bytes(field_name(table, count, index)) };
        append(graph, core::ptr::null_mut(), string_node(key));
    };
    if count as usize <= PACKED_FIELDS {
        while order != 0 {
            add((order & 15) - 1);
            order >>= 4;
        }
    } else if order != 0 {
        let length = word(node(order), 0);
        for position in 0..length {
            add(word(node(order), 4 * (position as usize + 1)));
        }
    }
    graph
}

/// Merge instance order while native field storage supplies the overlaid values.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_record_spread(
    target: *mut u8,
    source: *mut u8,
    table: *const u32,
    count: u32,
    source_table: *const u32,
    source_count: u32,
) {
    if count as usize <= PACKED_FIELDS
        && source_count as usize <= PACKED_FIELDS
        && word(target, record::JSON as usize) == 0
        && word(source, record::JSON as usize) == 0
    {
        let mut order = word(target, record::ORDER as usize);
        let mut incoming = word(source, record::ORDER as usize);
        while incoming != 0 {
            let key = unsafe {
                crate::runtime::bytes(field_name(source_table, source_count, (incoming & 15) - 1))
            };
            let index = unsafe {
                field_index(table, count, |name| {
                    key.len().cmp(&name.len()).then_with(|| key.cmp(name))
                })
            };
            if index < count {
                order = append_field(order, index);
            }
            incoming >>= 4;
        }
        set_word(target, record::ORDER as usize, order);
        return;
    }
    let base = unsafe { order_graph(target, table, count) };
    let source = unsafe { order_graph(source, source_table, source_count) };
    let graph = new_node(OBJECT);
    std_json_assign(graph, base);
    std_json_assign(graph, source);
    unsafe { std_json_record_retain(target, graph, table, count) };
}

/// Insert or remove a property without changing the positions of existing properties.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_record_field(
    native: *mut u8,
    table: *const u32,
    count: u32,
    key: *mut u8,
    present: u32,
) {
    let name = unsafe { crate::runtime::bytes(key) };
    let index = unsafe {
        field_index(table, count, |key| {
            name.len().cmp(&key.len()).then_with(|| name.cmp(key))
        })
    };
    if count as usize <= PACKED_FIELDS && index < count && word(native, record::JSON as usize) == 0
    {
        let order = word(native, record::ORDER as usize);
        let order = if present != 0 {
            append_field(order, index)
        } else {
            remove_field(order, index)
        };
        set_word(native, record::ORDER as usize, order);
        return;
    }
    let graph = unsafe { order_graph(native, table, count) };
    let graph = if present == 0 {
        unsafe { exclude_key(graph, key) }
    } else {
        let mut member = word(graph, FIRST);
        while member != 0 {
            if index < count && key_bytes(member) == name {
                return;
            }
            member = word(node(member), NEXT);
        }
        let out = new_node(OBJECT);
        std_json_assign(out, graph);
        append(out, core::ptr::null_mut(), string_node(name));
        out
    };
    unsafe { std_json_record_retain(native, graph, table, count) };
}
