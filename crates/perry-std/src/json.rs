//! JSON values (`JSON.parse`) and the writer behind typed `JSON.stringify`.
//!
//! Parsed values are nodes of the JSON helper's graph (`src/helpers/json`): `kind: u32` at
//! 0, `next: u32` at 4, the payload at 8 (a number, a boolean, or a string's data and byte
//! length), `first: u32` at 16, `count: u32` at 20, `key: u32` at 24 (a string node). The
//! null address is `undefined`.
//!
//! Object spreads (`{ ...a, ...b, k: v }`) build new objects whose members are shallow
//! copies of the sources' (values are immutable, so copies share their children). Members
//! keep the order the sources list them in; ECMAScript's property order (array-index keys
//! first, ascending, then the others in creation order, a repeated key keeping its first
//! position and its last value) is applied when serializing or enumerating ([canonical]).
//! Already ordered objects are marked to skip subsequent checks. Parsed objects go through
//! the same step, so duplicate and numeric keys come out as in Node.
mod reflection;
mod writer;

pub(crate) use reflection::std_json_strict_equal;
use reflection::{native_property, native_snapshot};

use crate::runtime::{Buffer, TYPE_ERROR, adopt, from_static, raise, scalars_length};

use perry_abi::json::{MEMBER_COUNT, kind::*};
use perry_abi::layout::descriptor;

/// Word 28 of an object: nonzero once its members are in property order.
const CANONICAL: usize = 28;
const NODE_BYTES: u32 = 32;

fn word(node: *mut u8, offset: usize) -> u32 {
    // SAFETY: compiled code passes graph nodes.
    unsafe { *(node.add(offset) as *const u32) }
}

fn node(address: u32) -> *mut u8 {
    address as usize as *mut u8
}

fn set_word(node: *mut u8, offset: usize, value: u32) {
    // SAFETY: callers pass nodes they own.
    unsafe { *(node.add(offset) as *mut u32) = value }
}

fn address(node: *mut u8) -> u32 {
    node as usize as u32
}

/// A fresh zeroed node of `kind`.
fn new_node(kind: u32) -> *mut u8 {
    let shape: &'static [u32] = match kind {
        STRING => &[0, 3, 4, 8, 24],
        EXCEPTION | NATIVE | ARRAY | OBJECT => &[0, 4, 4, 8, 16, 24],
        _ => &[0, 2, 4, 24],
    };
    // SAFETY: tags select exact pointer fields; scalar payload bits remain opaque.
    let node = unsafe { crate::runtime::alloc_shaped(NODE_BYTES, shape) };
    // SAFETY: a fresh block of `NODE_BYTES`.
    unsafe { core::ptr::write_bytes(node, 0, NODE_BYTES as usize) };
    set_word(node, 0, kind);
    node
}

/// A string node retaining its immutable UTF-8 payload through the exact pointer map.
fn string_node(bytes: &[u8]) -> *mut u8 {
    let node = new_node(STRING);
    set_word(node, 8, bytes.as_ptr() as usize as u32);
    set_word(node, 12, bytes.len() as u32);
    // Word 16 of a string node is its scalar count, as the parser writes it.
    set_word(node, 16, scalars_length(bytes));
    node
}

fn string_bytes(node: *mut u8) -> &'static [u8] {
    // SAFETY: string nodes hold a data address and byte length.
    unsafe { core::slice::from_raw_parts(self::node(word(node, 8)), word(node, 12) as usize) }
}

/// Reading a property of `undefined` or `null` throws, as in ECMAScript.
fn checked(value: *mut u8) -> bool {
    if value.is_null() || word(value, 0) == NULL {
        raise(TYPE_ERROR, "Cannot read properties of undefined or null");
        return false;
    }
    true
}

/// `value[name]`: the member, or `undefined`. Duplicate keys resolve to the last one.
///
/// # Safety
/// `name` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_member(value: *mut u8, name: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    property(value, unsafe { crate::runtime::bytes(name) })
}

fn property(value: *mut u8, name: &[u8]) -> *mut u8 {
    if !checked(value) {
        return core::ptr::null_mut();
    }
    match word(value, 0) {
        NATIVE => return native_property(value, name),
        EXCEPTION => {
            let error = node(word(value, 8));
            return match name {
                b"name" => unsafe {
                    std_json_from_string(crate::exception::std_exception_name(error))
                },
                b"message" => unsafe {
                    std_json_from_string(crate::runtime::exception_message(error))
                },
                b"cause" => unsafe {
                    *(error.add(crate::runtime::layout::exception::CAUSE as usize)
                        as *const *mut u8)
                },
                _ => core::ptr::null_mut(),
            };
        }
        ARRAY => {
            if name == b"length" {
                return std_json_from_number(f64::from(word(value, MEMBER_COUNT)));
            }
            return array_index(name)
                .map_or(core::ptr::null_mut(), |index| array_element(value, index));
        }
        STRING => {
            let bytes = string_bytes(value);
            if name == b"length" {
                return std_json_from_number(f64::from(scalars_length(bytes)));
            }
            // SAFETY: JSON string nodes contain validated UTF-8.
            let text = unsafe { core::str::from_utf8_unchecked(bytes) };
            return array_index(name)
                .and_then(|index| text.char_indices().nth(index as usize))
                .map_or(core::ptr::null_mut(), |(start, scalar)| {
                    string_node(&bytes[start..start + scalar.len_utf8()])
                });
        }
        OBJECT => {}
        _ => return core::ptr::null_mut(),
    }
    let mut found = core::ptr::null_mut();
    let mut child = word(value, 16);
    while child != 0 {
        let key = node(word(node(child), 24));
        if string_bytes(key) == name {
            found = node(child);
        }
        child = word(node(child), 4);
    }
    if !found.is_null() && word(found, 0) == UNDEFINED {
        return core::ptr::null_mut();
    }
    found
}

/// `value[index]` of an array, or `undefined`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_index(value: *mut u8, index: f64) -> *mut u8 {
    if !checked(value) {
        return core::ptr::null_mut();
    }
    if word(value, 0) != ARRAY {
        let mut text = [0; 40];
        let len = crate::runtime::number_format(index, &mut text);
        return property(value, &text[..len]);
    }
    if !(index >= 0.0
        && libm::trunc(index) == index
        && index < f64::from(word(value, MEMBER_COUNT)))
    {
        return core::ptr::null_mut();
    }
    array_element(value, index as u32)
}

fn array_element(value: *mut u8, index: u32) -> *mut u8 {
    if index >= word(value, MEMBER_COUNT) {
        return core::ptr::null_mut();
    }
    let mut child = word(value, 16);
    for _ in 0..index {
        child = word(node(child), 4);
    }
    let found = node(child);
    if word(found, 0) == UNDEFINED {
        core::ptr::null_mut()
    } else {
        found
    }
}

/// Own enumerable keys of an immutable JSON snapshot, in ECMAScript property order.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_keys(value: *mut u8) -> *mut u8 {
    if !checked(value) {
        return crate::runtime::array(&[]);
    }
    let value = if word(value, 0) == NATIVE {
        reflection::native_snapshot(value)
    } else {
        value
    };
    if word(value, 0) == OBJECT {
        let value = canonical(value);
        let mut child = word(value, 16);
        let mut count = 0;
        while child != 0 {
            count += usize::from(word(node(child), 0) != UNDEFINED);
            child = word(node(child), 4);
        }
        let (array, slots) = crate::runtime::array_storage(count);
        child = word(value, 16);
        let mut at = 0;
        while child != 0 {
            if word(node(child), 0) != UNDEFINED {
                slots[at] = std_json_to_string(node(word(node(child), 24))) as u64;
                at += 1;
            }
            child = word(node(child), 4);
        }
        return array;
    }
    let count = match word(value, 0) {
        ARRAY => word(value, MEMBER_COUNT),
        STRING => scalars_length(string_bytes(value)),
        _ => 0,
    };
    let (array, slots) = crate::runtime::array_storage(count as usize);
    for (index, slot) in slots.iter_mut().enumerate() {
        let mut text = [0; 40];
        let len = crate::runtime::number_format(index as f64, &mut text);
        *slot = crate::runtime::from_bytes(&text[..len]) as u64;
    }
    array
}

/// `value.length`: a string's Unicode scalar count or an array's count, else `undefined` (bits).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_length(value: *mut u8) -> u64 {
    if !checked(value) {
        return 0;
    }
    match word(value, 0) {
        STRING => f64::from(scalars_length(string_bytes(value))).to_bits(),
        ARRAY => f64::from(word(value, MEMBER_COUNT)).to_bits(),
        NATIVE if word(node(word(value, 12)), 0) == descriptor::ARRAY => {
            f64::from(word(node(word(value, 8)), 0)).to_bits()
        }
        _ => crate::runtime::UNDEFINED_BITS,
    }
}

fn expected(value: *mut u8, kind: u32, what: &'static str) -> bool {
    if value.is_null() || word(value, 0) != kind {
        raise(TYPE_ERROR, what);
        return false;
    }
    true
}

/// A JSON value used as `number`: ECMAScript `ToNumber` for numbers, `null` (0), booleans
/// and `undefined` (NaN), so arithmetic on `as number` values matches; strings, arrays and
/// objects throw `TypeError` (R6: no implicit parsing of untyped data).
/// # Safety
/// `value` must be a JSON node or null.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_to_number(value: *mut u8) -> f64 {
    if value.is_null() {
        return f64::NAN;
    }
    match word(value, 0) {
        // SAFETY: number nodes hold an f64 at 8.
        NUMBER => unsafe { *(value.add(8) as *const f64) },
        INT64 => unsafe { *(value.add(8) as *const i64) as f64 },
        UINT64 => unsafe { *(value.add(8) as *const u64) as f64 },
        NULL => 0.0,
        BOOLEAN => f64::from(word(value, 8)),
        _ => {
            raise(TYPE_ERROR, "JSON value is not a number");
            0.0
        }
    }
}

/// A checked JSON number at a typed boundary.
///
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_require_number(value: *mut u8) -> f64 {
    if !value.is_null() {
        match word(value, 0) {
            NUMBER => return unsafe { *(value.add(8) as *const f64) },
            INT64 => return unsafe { *(value.add(8) as *const i64) as f64 },
            UINT64 => return unsafe { *(value.add(8) as *const u64) as f64 },
            _ => {}
        }
    }
    raise(TYPE_ERROR, "JSON value is not a number");
    0.0
}

/// A checked signed 64-bit integer at a typed JSON boundary.
///
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_to_i64(value: *mut u8) -> i64 {
    if !value.is_null() {
        match word(value, 0) {
            INT64 => return unsafe { *(value.add(8) as *const i64) },
            UINT64 => {
                let u = unsafe { *(value.add(8) as *const u64) };
                if let Ok(i) = i64::try_from(u) {
                    return i;
                }
            }
            NUMBER => {
                let n = unsafe { *(value.add(8) as *const f64) };
                if n.is_finite()
                    && libm::trunc(n) == n
                    && (-(1i64 << 53) as f64..=(1i64 << 53) as f64).contains(&n)
                {
                    return n as i64;
                }
            }
            _ => {}
        }
    }
    raise(TYPE_ERROR, "JSON value does not match the declared schema");
    0
}

/// A checked unsigned 64-bit integer at a typed JSON boundary.
///
/// # Safety
/// `value` is null (undefined) or a live JSON node.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_to_u64(value: *mut u8) -> u64 {
    if !value.is_null() {
        match word(value, 0) {
            UINT64 => return unsafe { *(value.add(8) as *const u64) },
            INT64 => {
                let i = unsafe { *(value.add(8) as *const i64) };
                if let Ok(u) = u64::try_from(i) {
                    return u;
                }
            }
            NUMBER => {
                let n = unsafe { *(value.add(8) as *const f64) };
                if n.is_finite() && libm::trunc(n) == n && (0.0..=(1u64 << 53) as f64).contains(&n)
                {
                    return n as u64;
                }
            }
            _ => {}
        }
    }
    raise(TYPE_ERROR, "JSON value does not match the declared schema");
    0
}

/// A JSON string used as `string`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_to_string(value: *mut u8) -> *mut u8 {
    if !expected(value, STRING, "JSON value is not a string") {
        return from_static("");
    }
    let bytes = string_bytes(value);
    // SAFETY: the parser validated the UTF-8; the bytes live in the task region.
    unsafe { adopt(bytes.as_ptr() as *mut u8, bytes.len() as u32) }
}

/// A JSON boolean used as `boolean`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_to_bool(value: *mut u8) -> u32 {
    if !expected(value, BOOLEAN, "JSON value is not a boolean") {
        return 0;
    }
    word(value, 8)
}

/// ECMAScript truthiness within the immutable JSON value domain.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_truthy(value: *mut u8) -> u32 {
    if value.is_null() {
        return 0;
    }
    match word(value, 0) {
        UNDEFINED | NULL => 0,
        BOOLEAN => word(value, 8),
        NUMBER => {
            let bits = u64::from(word(value, 8)) | (u64::from(word(value, 12)) << 32);
            let number = f64::from_bits(bits);
            u32::from(number != 0.0 && !number.is_nan())
        }
        INT64 | UINT64 => u32::from(word(value, 8) != 0 || word(value, 12) != 0),
        STRING => u32::from(word(value, 12) != 0),
        ARRAY | OBJECT | EXCEPTION | NATIVE => 1,
        _ => unreachable!("JSON tag"),
    }
}

fn append_number(text: &mut Buffer, number: f64) {
    if number.is_finite() {
        let mut digits = [0; 40];
        let len = crate::runtime::number_format(number, &mut digits);
        text.extend_from_slice(&digits[..len]);
    } else {
        text.extend_from_slice(b"null");
    }
}

pub(super) fn format_integer(mut magnitude: u64, negative: bool, digits: &mut [u8; 21]) -> &[u8] {
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (magnitude % 10) as u8;
        magnitude /= 10;
        if magnitude == 0 {
            break;
        }
    }
    if negative {
        at -= 1;
        digits[at] = b'-';
    }
    &digits[at..]
}

pub(super) fn append_i64(text: &mut Buffer, value: i64) {
    let mut digits = [0u8; 21];
    text.extend_from_slice(format_integer(value.unsigned_abs(), value < 0, &mut digits));
}

pub(super) fn append_u64(text: &mut Buffer, value: u64) {
    let mut digits = [0u8; 21];
    text.extend_from_slice(format_integer(value, false, &mut digits));
}

/// `typeof value`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_type_of(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return from_static("undefined");
    }
    from_static(match word(value, 0) {
        BOOLEAN => "boolean",
        NUMBER | INT64 | UINT64 => "number",
        STRING => "string",
        _ => "object",
    })
}

/// Whether a JSON node is the explicit `null` value, distinct from an absent property.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_is_null(value: *mut u8) -> u32 {
    u32::from(!value.is_null() && word(value, 0) == NULL)
}

/// The array index a key names (a canonical decimal below 2³² − 1), if any.
fn array_index(key: &[u8]) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key[0] == b'0' && key.len() > 1) {
        return None;
    }
    let mut value = 0u64;
    for &digit in key {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u64::from(digit - b'0');
    }
    (value < u64::from(u32::MAX)).then_some(value as u32)
}

fn key_bytes(member: u32) -> &'static [u8] {
    string_bytes(node(word(node(member), 24)))
}

/// An object view with its members in ECMAScript property order: array-index keys first in
/// ascending order, then the others in creation order; a repeated key keeps its first
/// position and takes its last value. Reordered members belong to a fresh traced object.
/// The original may live in an opaque parsed graph, which cannot trace external nodes.
fn canonical(object: *mut u8) -> *mut u8 {
    if word(object, CANONICAL) != 0 {
        return object;
    }
    let count = word(object, MEMBER_COUNT) as usize;
    if count < 2 {
        set_word(object, CANONICAL, 1);
        return object;
    }
    let size = (count * 2).next_power_of_two();
    let words = count * 2 + size;
    let mut local = [0u32; 128];
    let scratch = if words <= local.len() {
        &mut local[..words]
    } else {
        let data = crate::runtime::alloc((words * 4) as u32) as *mut u32;
        // SAFETY: a fresh block of `words` words, with no collection during canonicalization.
        unsafe {
            core::ptr::write_bytes(data, 0, words);
            core::slice::from_raw_parts_mut(data, words)
        }
    };
    // Each entry keeps its first insertion position and its final member node.
    let (entries, table) = scratch.split_at_mut(count * 2);
    let entries = entries.as_chunks_mut::<2>().0;
    let mut unique = 0;
    let mut child = word(object, 16);
    let mut changed = false;
    while child != 0 {
        let key = key_bytes(child);
        let mut hash = 0x811c_9dc5u32;
        for &byte in key {
            hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
        }
        let mut at = hash as usize & (size - 1);
        loop {
            match table[at] {
                0 => {
                    table[at] = unique as u32 + 1;
                    entries[unique] = [unique as u32, child];
                    unique += 1;
                    break;
                }
                slot if key_bytes(entries[slot as usize - 1][1]) == key => {
                    entries[slot as usize - 1][1] = child;
                    changed = true;
                    break;
                }
                _ => at = (at + 1) & (size - 1),
            }
        }
        child = word(node(child), 4);
    }
    let entries = &mut entries[..unique];
    let order = |entry: [u32; 2]| match array_index(key_bytes(entry[1])) {
        Some(index) => (0, index),
        None => (1, entry[0]),
    };
    if !entries
        .windows(2)
        .all(|pair| order(pair[0]) <= order(pair[1]))
    {
        heap_sort(entries, |a, b| order(*a).cmp(&order(*b)));
        changed = true;
    }
    if !changed {
        set_word(object, CANONICAL, 1);
        return object;
    }
    let ordered = new_node(OBJECT);
    let mut previous: *mut u8 = core::ptr::null_mut();
    for &[_, member] in entries.iter() {
        let copy = new_node(word(node(member), 0));
        // SAFETY: both are `NODE_BYTES` blocks.
        unsafe { core::ptr::copy_nonoverlapping(node(member), copy, NODE_BYTES as usize) };
        set_word(copy, 4, 0);
        if previous.is_null() {
            set_word(ordered, 16, address(copy));
        } else {
            set_word(previous, 4, address(copy));
        }
        previous = copy;
    }
    set_word(ordered, MEMBER_COUNT, unique as u32);
    set_word(ordered, CANONICAL, 1);
    ordered
}

/// Bounded-stack heapsort; the linked helper must have no recursive call paths.
fn heap_sort<T>(items: &mut [T], compare: impl Fn(&T, &T) -> core::cmp::Ordering) {
    let sift = |items: &mut [T], mut root: usize| {
        while root < items.len() / 2 {
            let mut child = root * 2 + 1;
            if child + 1 < items.len() && compare(&items[child], &items[child + 1]).is_lt() {
                child += 1;
            }
            if !compare(&items[root], &items[child]).is_lt() {
                break;
            }
            items.swap(root, child);
            root = child;
        }
    };
    for root in (0..items.len() / 2).rev() {
        sift(items, root);
    }
    for end in (1..items.len()).rev() {
        items.swap(0, end);
        sift(&mut items[..end], 0);
    }
}

/// Append a shallow copy of `value` (or an `undefined` member) under `key` to an object
/// under construction; word 8 tracks its last member.
fn append(object: *mut u8, value: *mut u8, key: *mut u8) {
    let copy = if value.is_null() {
        new_node(UNDEFINED)
    } else {
        let copy = new_node(word(value, 0));
        // SAFETY: both are `NODE_BYTES` blocks.
        unsafe { core::ptr::copy_nonoverlapping(value, copy, NODE_BYTES as usize) };
        set_word(copy, 4, 0);
        copy
    };
    set_word(copy, 24, address(key));
    match word(object, 8) {
        0 => set_word(object, 16, address(copy)),
        last => set_word(node(last), 4, address(copy)),
    }
    set_word(object, 8, address(copy));
    set_word(object, MEMBER_COUNT, word(object, MEMBER_COUNT) + 1);
    set_word(object, CANONICAL, 0);
}

/// A key node for array index `index`.
fn index_key(index: u32) -> *mut u8 {
    let mut digits = [0u8; 10];
    let mut at = digits.len();
    let mut rest = index;
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    let data = crate::runtime::alloc((digits.len() - at) as u32);
    // SAFETY: a fresh block of the digits' length.
    let text = unsafe {
        core::ptr::copy_nonoverlapping(digits[at..].as_ptr(), data, digits.len() - at);
        core::slice::from_raw_parts(data, digits.len() - at)
    };
    string_node(text)
}

/// `{}`: the object a spread literal starts from.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_object() -> *mut u8 {
    new_node(OBJECT)
}

/// `...source` in an object literal: an object's members, an array's elements or a string's
/// characters under their indices; other values add nothing.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_assign(object: *mut u8, source: *mut u8) {
    if source.is_null() {
        return;
    }
    let source = if word(source, 0) == NATIVE {
        reflection::native_snapshot(source)
    } else {
        source
    };
    match word(source, 0) {
        OBJECT => {
            let mut child = word(source, 16);
            while child != 0 {
                append(object, node(child), node(word(node(child), 24)));
                child = word(node(child), 4);
            }
        }
        ARRAY => {
            let mut child = word(source, 16);
            let mut index = 0;
            while child != 0 {
                append(object, node(child), index_key(index));
                index += 1;
                child = word(node(child), 4);
            }
        }
        STRING => {
            let text = string_bytes(source);
            let (mut at, mut index) = (0, 0);
            // SAFETY: JSON string nodes contain validated UTF-8.
            for character in unsafe { core::str::from_utf8_unchecked(text) }.chars() {
                let width = character.len_utf8();
                append(object, string_node(&text[at..at + width]), index_key(index));
                at += width;
                index += 1;
            }
        }
        _ => {}
    }
}

/// `key: value` in an object literal with spreads.
///
/// # Safety
/// `key` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_set(object: *mut u8, key: *mut u8, value: *mut u8) {
    // SAFETY: guaranteed by the caller; strings live in the task region.
    let key = string_node(unsafe { crate::runtime::bytes(key) });
    append(object, value, key);
}

/// A number as a JSON value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_number(value: f64) -> *mut u8 {
    let node = new_node(NUMBER);
    // SAFETY: number nodes hold an f64 at 8.
    unsafe { *(node.add(8) as *mut f64) = value };
    node
}

/// A signed 64-bit integer as an exact JSON value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_i64(value: i64) -> *mut u8 {
    let node = new_node(INT64);
    // SAFETY: INT64 nodes hold an i64 at 8.
    unsafe { *(node.add(8) as *mut i64) = value };
    node
}

/// An unsigned 64-bit integer as an exact JSON value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_u64(value: u64) -> *mut u8 {
    let node = new_node(UINT64);
    // SAFETY: UINT64 nodes hold a u64 at 8.
    unsafe { *(node.add(8) as *mut u64) = value };
    node
}

/// A string as a JSON value.
///
/// # Safety
/// `value` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_from_string(value: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller; strings live in the task region.
    let bytes = unsafe { crate::runtime::bytes(value) };
    let scalars = unsafe { (*value.cast::<perry_abi::layout::string::Header>()).scalars };
    let node = new_node(STRING);
    set_word(node, 8, bytes.as_ptr() as usize as u32);
    set_word(node, 12, bytes.len() as u32);
    set_word(node, 16, scalars);
    node
}

/// A boolean (`kind` 2) or `null` (`kind` 1) as a JSON value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_from_scalar(kind: u32, truth: u32) -> *mut u8 {
    let node = new_node(kind);
    set_word(node, 8, truth);
    node
}

/// Check the container before a closed schema reads its declared fields.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_require_object(value: *mut u8) {
    expected(value, OBJECT, "JSON value is not an object");
}

/// Start linear traversal of a checked JSON array.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_first(value: *mut u8) -> *mut u8 {
    if !expected(value, ARRAY, "JSON value is not an array") {
        return core::ptr::null_mut();
    }
    node(word(value, 16))
}

/// Advance a checked array traversal. Null marks the end, without allocating iterator state.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_next(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        core::ptr::null_mut()
    } else {
        node(word(value, 4))
    }
}
