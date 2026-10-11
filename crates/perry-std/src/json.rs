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
//! position and its last value) is applied once per object when it is first serialized or
//! spread ([canonical]). Parsed objects go through the same step, so duplicate and numeric
//! keys come out as in Node.
use crate::runtime::{Buffer, TYPE_ERROR, adopt, from_static, raise, scalars_length};

const NULL: u32 = 1;
const BOOLEAN: u32 = 2;
const NUMBER: u32 = 3;
const STRING: u32 = 4;
const ARRAY: u32 = 5;
const OBJECT: u32 = 6;
/// A member a spread set to `undefined`: it hides earlier values and is not serialized.
const UNDEFINED: u32 = 0;
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
        ARRAY | OBJECT => &[0, 4, 4, 8, 16, 24],
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
    set_word(
        node,
        16,
        bytes.iter().filter(|&&b| b & 0xC0 != 0x80).count() as u32,
    );
    node
}

/// Scratch words in the task region.
fn words(count: usize) -> &'static mut [u32] {
    if count == 0 {
        return &mut [];
    }
    let data = crate::runtime::alloc((count * 4) as u32) as *mut u32;
    // SAFETY: a fresh block of `count` words.
    unsafe {
        core::ptr::write_bytes(data, 0, count);
        core::slice::from_raw_parts_mut(data, count)
    }
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
        ARRAY => {
            if name == b"length" {
                return std_json_from_number(f64::from(word(value, 20)));
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
    if !(index >= 0.0 && libm::trunc(index) == index && index < f64::from(word(value, 20))) {
        return core::ptr::null_mut();
    }
    array_element(value, index as u32)
}

fn array_element(value: *mut u8, index: u32) -> *mut u8 {
    if index >= word(value, 20) {
        return core::ptr::null_mut();
    }
    let mut child = word(value, 16);
    for _ in 0..index {
        child = word(node(child), 4);
    }
    node(child)
}

/// Own enumerable keys of an immutable JSON snapshot, in ECMAScript property order.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_keys(value: *mut u8) -> *mut u8 {
    if !checked(value) {
        return crate::runtime::array(&[]);
    }
    if word(value, 0) == OBJECT {
        canonical(value);
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
        ARRAY => word(value, 20),
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
        ARRAY => f64::from(word(value, 20)).to_bits(),
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
    if !expected(value, NUMBER, "JSON value is not a number") {
        return 0.0;
    }
    // SAFETY: the checked number node holds an f64 at offset 8.
    unsafe { *(value.add(8) as *const f64) }
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
        STRING => u32::from(word(value, 12) != 0),
        ARRAY | OBJECT => 1,
        _ => unreachable!("JSON tag"),
    }
}

/// The writer of `JSON.stringify` for typed values: compiled serializers call it with
/// structure and pre-formatted pieces.
#[repr(C)]
pub(super) struct Writer {
    pub(super) text: Buffer,
    /// Bit `d` set: the container at depth `d` already has an element.
    started: [u64; 2],
    pub(super) depth: u32,
    pub(super) max_depth: u32,
    pub(super) failed: bool,
}

/// Nesting deeper than this means a cycle (or absurd data): `TypeError`, as for cycles.
pub(super) const MAX_DEPTH: u32 = 128;

pub(super) fn writer(w: *mut u8) -> &'static mut Writer {
    // SAFETY: compiled code passes writers from [std_json_writer].
    unsafe { &mut *(w as *mut Writer) }
}

/// A new writer in the task region.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_writer() -> *mut u8 {
    let w = perry_abi::object!(Writer; text);
    // SAFETY: freshly allocated.
    unsafe {
        w.write(Writer {
            text: Buffer::new(),
            started: [0; 2],
            depth: 0,
            max_depth: 0,
            failed: false,
        })
    };
    w as *mut u8
}

/// Enter a container value; returns 0 (and fails the writer) when nesting is too deep.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_open(w: *mut u8, bracket: u32) -> u32 {
    let w = writer(w);
    if w.failed || w.depth >= MAX_DEPTH {
        w.failed = true;
        return 0;
    }
    w.text.push(bracket as u8);
    w.depth += 1;
    w.max_depth = w.max_depth.max(w.depth);
    let (word, bit) = (((w.depth - 1) / 64) as usize, (w.depth - 1) % 64);
    w.started[word] &= !(1 << bit);
    1
}

/// Leave a container value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_close(w: *mut u8, bracket: u32) {
    let w = writer(w);
    if w.failed {
        return;
    }
    w.text.push(bracket as u8);
    w.depth -= 1;
}

fn separate(w: &mut Writer) {
    let (word, bit) = (((w.depth - 1) / 64) as usize, (w.depth - 1) % 64);
    if w.started[word] & (1 << bit) != 0 {
        w.text.push(b',');
    }
    w.started[word] |= 1 << bit;
}

/// Start an array element.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_item(w: *mut u8) {
    let w = writer(w);
    if !w.failed {
        separate(w);
    }
}

/// Start an object member; `key` is the already quoted key followed by `:`.
///
/// # Safety
/// `key` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_key(w: *mut u8, key: *mut u8) {
    let w = writer(w);
    if !w.failed {
        separate(w);
        // SAFETY: guaranteed by the caller.
        w.text
            .extend_from_slice(unsafe { crate::runtime::bytes(key) });
    }
}

/// Append raw text (a number's digits, `null`, `true`, `false`).
/// # Safety
/// `data` must point to `len` readable bytes.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_raw(w: *mut u8, data: *const u8, len: u32) {
    let w = writer(w);
    if !w.failed && len > 0 {
        // SAFETY: compiled code passes `len` readable bytes.
        w.text
            .extend_from_slice(unsafe { core::slice::from_raw_parts(data, len as usize) });
    }
}

/// Append an ECMAScript number without allocating a per-number scratch object.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_number(w: *mut u8, number: f64) {
    let w = writer(w);
    if !w.failed {
        append_number(&mut w.text, number);
    }
}

pub(super) fn append_number(text: &mut Buffer, number: f64) {
    if number.is_finite() {
        let mut digits = [0; 40];
        let len = crate::runtime::number_format(number, &mut digits);
        text.extend_from_slice(&digits[..len]);
    } else {
        text.extend_from_slice(b"null");
    }
}

/// Append a string as a JSON string literal (ECMAScript `QuoteJSONString`).
///
/// # Safety
/// `string` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_quote(w: *mut u8, string: *mut u8) {
    let w = writer(w);
    if w.failed {
        return;
    }
    // SAFETY: guaranteed by the caller.
    quote(&mut w.text, unsafe { crate::runtime::bytes(string) });
}

fn unescaped_prefix(bytes: &[u8]) -> usize {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let zero = |word: u64| word.wrapping_sub(ONES) & !word & HIGHS;
    let mut index = 0;
    while index + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[index..index + 8].try_into().unwrap());
        if (word.wrapping_sub(ONES * 32) & !word & HIGHS)
            | zero(word ^ (ONES * u64::from(b'"')))
            | zero(word ^ (ONES * u64::from(b'\\')))
            != 0
        {
            break;
        }
        index += 8;
    }
    while index < bytes.len() && bytes[index] >= 32 && !matches!(bytes[index], b'"' | b'\\') {
        index += 1;
    }
    index
}

pub(crate) fn quote(text: &mut Buffer, bytes: &[u8]) {
    text.push(b'"');
    quote_contents(text, bytes);
    text.push(b'"');
}

pub(super) fn quote_contents(text: &mut Buffer, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        index += unescaped_prefix(&bytes[index..]);
        if index == bytes.len() {
            break;
        }
        let byte = bytes[index];
        let escape: &[u8] = match byte {
            b'"' => br#"\""#,
            b'\\' => br"\\",
            0x08 => br"\b",
            0x0C => br"\f",
            b'\n' => br"\n",
            b'\r' => br"\r",
            b'\t' => br"\t",
            0..=0x1F => &[],
            _ => unreachable!("the prefix ends at an escape byte"),
        };
        text.extend_from_slice(&bytes[start..index]);
        if escape.is_empty() {
            text.extend_from_slice(br"\u00");
            text.push(HEX[usize::from(byte >> 4)]);
            text.push(HEX[usize::from(byte & 15)]);
        } else {
            text.extend_from_slice(escape);
        }
        index += 1;
        start = index;
    }
    text.extend_from_slice(&bytes[start..]);
}

/// The text written, or null after a failure (with a `TypeError` raised).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_finish(w: *mut u8) -> *mut u8 {
    let w = writer(w);
    if w.failed {
        raise(TYPE_ERROR, "Converting circular structure to JSON");
        return from_static("");
    }
    let text = core::mem::take(&mut w.text);
    let bytes = text.as_slice();
    // SAFETY: finished UTF-8 output is immutable; the string traces this owned allocation.
    unsafe { adopt(bytes.as_ptr().cast_mut(), bytes.len() as u32) }
}

/// `typeof value`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_type_of(value: *mut u8) -> *mut u8 {
    if value.is_null() {
        return from_static("undefined");
    }
    from_static(match word(value, 0) {
        BOOLEAN => "boolean",
        NUMBER => "number",
        STRING => "string",
        _ => "object",
    })
}

/// Whether a JSON node is the explicit `null` value, distinct from an absent property.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_is_null(value: *mut u8) -> u32 {
    u32::from(!value.is_null() && word(value, 0) == NULL)
}

/// Write a parsed JSON value starting `depth` levels deep; false when it nests deeper than
/// [MAX_DEPTH]. Iterative, with a fixed stack of open containers, so the linker can bound
/// the stack of programs that serialize JSON values.
fn value_text(text: &mut Buffer, value: *mut u8, depth: u32) -> bool {
    /// An array or object being written: its node, its next member, and whether a member
    /// was written (for separators).
    #[derive(Clone, Copy)]
    struct Open {
        node: *mut u8,
        next: u32,
        written: bool,
    }
    let mut stack = [Open {
        node: core::ptr::null_mut(),
        next: 0,
        written: false,
    }; MAX_DEPTH as usize + 1];
    let mut open = 0usize;
    let mut current = value;
    loop {
        if depth + open as u32 > MAX_DEPTH {
            return false;
        }
        match word(current, 0) {
            NULL => text.extend_from_slice(b"null"),
            BOOLEAN => text.extend_from_slice(if word(current, 8) != 0 {
                b"true"
            } else {
                b"false"
            }),
            NUMBER => {
                // SAFETY: number nodes hold an f64 at 8.
                let number = unsafe { *(current.add(8) as *const f64) };
                append_number(text, number);
            }
            STRING => quote(text, string_bytes(current)),
            kind @ (ARRAY | OBJECT) => {
                if kind == OBJECT {
                    canonical(current);
                }
                text.push(if kind == ARRAY { b'[' } else { b'{' });
                stack[open] = Open {
                    node: current,
                    next: word(current, 16),
                    written: false,
                };
                open += 1;
            }
            _ => return false,
        }
        // The next member to write, closing the containers that are done.
        loop {
            let Some(top) = open.checked_sub(1) else {
                return true;
            };
            let container = stack[top];
            let mut member = container.next;
            while member != 0 && word(node(member), 0) == UNDEFINED {
                member = word(node(member), 4);
            }
            let object = word(container.node, 0) == OBJECT;
            if member == 0 {
                text.push(if object { b'}' } else { b']' });
                open = top;
                continue;
            }
            if container.written {
                text.push(b',');
            }
            if object {
                quote(text, string_bytes(node(word(node(member), 24))));
                text.push(b':');
            }
            stack[top].next = word(node(member), 4);
            stack[top].written = true;
            current = node(member);
            break;
        }
    }
}

/// Append a parsed JSON value (`JSON.stringify` of `JSON.parse` output; `undefined` writes
/// nothing, so callers check it first).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_value(w: *mut u8, value: *mut u8) {
    let w = writer(w);
    if w.failed || value.is_null() {
        return;
    }
    if !value_text(&mut w.text, value, w.depth) {
        w.failed = true;
    }
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

/// Put an object's members in ECMAScript property order, once: array-index keys first in
/// ascending order, then the others in creation order; a repeated key keeps its first
/// position and takes its last value. Reordered members are copied, so shallow copies of
/// the object that still share the old member list are unaffected.
fn canonical(object: *mut u8) {
    if word(object, CANONICAL) != 0 {
        return;
    }
    set_word(object, CANONICAL, 1);
    let count = word(object, 20) as usize;
    if count < 2 {
        return;
    }
    let members = words(count);
    let mut child = word(object, 16);
    for slot in members.iter_mut() {
        *slot = child;
        child = word(node(child), 4);
    }
    // Deduplicate through an open-addressing table of entry positions (+1).
    let size = (count * 2).next_power_of_two();
    let table = words(size);
    let entries = words(count);
    let mut unique = 0;
    for &member in members.iter() {
        let key = key_bytes(member);
        let mut hash = 0x811c_9dc5u32;
        for &byte in key {
            hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
        }
        let mut at = hash as usize & (size - 1);
        loop {
            match table[at] {
                0 => {
                    table[at] = unique as u32 + 1;
                    entries[unique] = member;
                    unique += 1;
                    break;
                }
                slot if key_bytes(entries[slot as usize - 1]) == key => {
                    entries[slot as usize - 1] = member;
                    break;
                }
                _ => at = (at + 1) & (size - 1),
            }
        }
    }
    let entries = &mut entries[..unique];
    // Array-index keys move to the front in ascending order (keys are unique now).
    let indexed = words(unique * 2);
    let mut indices = 0;
    for (position, &member) in entries.iter().enumerate() {
        if let Some(index) = array_index(key_bytes(member)) {
            indexed[indices * 2] = index;
            indexed[indices * 2 + 1] = position as u32;
            indices += 1;
        }
    }
    let order = words(unique);
    if indices > 0 {
        // SAFETY: pairs of words reinterpreted as `u64` (index in the high word).
        let pairs =
            unsafe { core::slice::from_raw_parts_mut(indexed.as_mut_ptr() as *mut u64, indices) };
        for pair in pairs.iter_mut() {
            *pair = pair.rotate_left(32);
        }
        heapsort(pairs);
        for (slot, pair) in order.iter_mut().zip(pairs.iter()) {
            *slot = entries[*pair as u32 as usize];
        }
    }
    let mut next = indices;
    for &member in entries.iter() {
        if array_index(key_bytes(member)).is_none() {
            order[next] = member;
            next += 1;
        }
    }
    if order[..] == members[..] {
        return;
    }
    let mut previous: *mut u8 = core::ptr::null_mut();
    for &member in order.iter() {
        let copy = new_node(word(node(member), 0));
        // SAFETY: both are `NODE_BYTES` blocks.
        unsafe { core::ptr::copy_nonoverlapping(node(member), copy, NODE_BYTES as usize) };
        set_word(copy, 4, 0);
        if previous.is_null() {
            set_word(object, 16, address(copy));
        } else {
            set_word(previous, 4, address(copy));
        }
        previous = copy;
    }
    set_word(object, 20, unique as u32);
}

/// In-place heapsort: `O(n log n)` like `sort_unstable`, at a tenth of its code.
fn heapsort(values: &mut [u64]) {
    fn sift(values: &mut [u64], mut root: usize, end: usize) {
        loop {
            let mut child = 2 * root + 1;
            if child >= end {
                return;
            }
            if child + 1 < end && values[child] < values[child + 1] {
                child += 1;
            }
            if values[root] >= values[child] {
                return;
            }
            values.swap(root, child);
            root = child;
        }
    }
    let len = values.len();
    for root in (0..len / 2).rev() {
        sift(values, root, len);
    }
    for end in (1..len).rev() {
        values.swap(0, end);
        sift(values, 0, end);
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
    set_word(object, 20, word(object, 20) + 1);
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
            for character in core::str::from_utf8(text).unwrap_or("").chars() {
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

/// A string as a JSON value.
///
/// # Safety
/// `value` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_from_string(value: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller; strings live in the task region.
    string_node(unsafe { crate::runtime::bytes(value) })
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
