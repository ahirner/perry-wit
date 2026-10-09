//! JSON values (`JSON.parse`) and the writer behind typed `JSON.stringify`.
//!
//! Parsed values are nodes of the JSON helper's graph (`src/helpers/json`): `kind: u32` at
//! 0, `next: u32` at 4, the payload at 8 (a number, a boolean, or a string's data and byte
//! length), `first: u32` at 16, `count: u32` at 20, `key: u32` at 24 (a string node). The
//! null address is `undefined`.
use crate::runtime::{Buffer, TYPE_ERROR, adopt, from_static, raise, utf16_length};

const NULL: u32 = 1;
const BOOLEAN: u32 = 2;
const NUMBER: u32 = 3;
const STRING: u32 = 4;
const ARRAY: u32 = 5;
const OBJECT: u32 = 6;

fn word(node: *mut u8, offset: usize) -> u32 {
    // SAFETY: compiled code passes graph nodes.
    unsafe { *(node.add(offset) as *const u32) }
}

fn node(address: u32) -> *mut u8 {
    address as usize as *mut u8
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
pub unsafe extern "C" fn std_json_member(value: *mut u8, name: *mut u8) -> *mut u8 {
    if !checked(value) || word(value, 0) != OBJECT {
        return core::ptr::null_mut();
    }
    // SAFETY: guaranteed by the caller.
    let name = unsafe { crate::runtime::bytes(name) };
    let mut found = core::ptr::null_mut();
    let mut child = word(value, 16);
    while child != 0 {
        let key = node(word(node(child), 24));
        if string_bytes(key) == name {
            found = node(child);
        }
        child = word(node(child), 4);
    }
    found
}

/// `value[index]` of an array, or `undefined`.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_index(value: *mut u8, index: f64) -> *mut u8 {
    if !checked(value) || word(value, 0) != ARRAY {
        return core::ptr::null_mut();
    }
    if !(index >= 0.0 && libm::trunc(index) == index && index < f64::from(word(value, 20))) {
        return core::ptr::null_mut();
    }
    let mut child = word(value, 16);
    for _ in 0..index as u32 {
        child = word(node(child), 4);
    }
    node(child)
}

/// `value.length`: a string's UTF-16 length or an array's count, else `undefined` (bits).
#[unsafe(no_mangle)]
pub extern "C" fn std_json_length(value: *mut u8) -> u64 {
    if !checked(value) {
        return 0;
    }
    match word(value, 0) {
        STRING => f64::from(utf16_length(string_bytes(value))).to_bits(),
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
pub unsafe extern "C" fn std_json_to_number(value: *mut u8) -> f64 {
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

/// A JSON string used as `string`.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_to_string(value: *mut u8) -> *mut u8 {
    if !expected(value, STRING, "JSON value is not a string") {
        return from_static("");
    }
    let bytes = string_bytes(value);
    // SAFETY: the parser validated the UTF-8; the bytes live in the task region.
    unsafe { adopt(bytes.as_ptr() as *mut u8, bytes.len() as u32) }
}

/// A JSON boolean used as `boolean`.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_to_bool(value: *mut u8) -> u32 {
    if !expected(value, BOOLEAN, "JSON value is not a boolean") {
        return 0;
    }
    word(value, 8)
}

/// The writer of `JSON.stringify` for typed values: compiled serializers call it with
/// structure and pre-formatted pieces.
#[repr(C)]
pub struct Writer {
    text: Buffer,
    /// Bit `d` set: the container at depth `d` already has an element.
    started: [u64; 2],
    depth: u32,
    failed: bool,
}

/// Nesting deeper than this means a cycle (or absurd data): `TypeError`, as for cycles.
const MAX_DEPTH: u32 = 128;

fn writer(w: *mut u8) -> &'static mut Writer {
    // SAFETY: compiled code passes writers from [std_json_writer].
    unsafe { &mut *(w as *mut Writer) }
}

/// A new writer in the task region.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_writer() -> *mut u8 {
    let w = crate::runtime::alloc(core::mem::size_of::<Writer>() as u32) as *mut Writer;
    // SAFETY: freshly allocated.
    unsafe {
        w.write(Writer {
            text: Buffer::new(),
            started: [0; 2],
            depth: 0,
            failed: false,
        })
    };
    w as *mut u8
}

/// Enter a container value; returns 0 (and fails the writer) when nesting is too deep.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_open(w: *mut u8, bracket: u32) -> u32 {
    let w = writer(w);
    if w.failed || w.depth >= MAX_DEPTH {
        w.failed = true;
        return 0;
    }
    w.text.push(bracket as u8);
    w.depth += 1;
    let (word, bit) = ((w.depth / 64) as usize, w.depth % 64);
    w.started[word] &= !(1 << bit);
    1
}

/// Leave a container value.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_close(w: *mut u8, bracket: u32) {
    let w = writer(w);
    if w.failed {
        return;
    }
    w.text.push(bracket as u8);
    w.depth -= 1;
}

fn separate(w: &mut Writer) {
    let (word, bit) = ((w.depth / 64) as usize, w.depth % 64);
    if w.started[word] & (1 << bit) != 0 {
        w.text.push(b',');
    }
    w.started[word] |= 1 << bit;
}

/// Start an array element.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_item(w: *mut u8) {
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
pub unsafe extern "C" fn std_json_key(w: *mut u8, key: *mut u8) {
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
pub unsafe extern "C" fn std_json_raw(w: *mut u8, data: *const u8, len: u32) {
    let w = writer(w);
    if !w.failed && len > 0 {
        // SAFETY: compiled code passes `len` readable bytes.
        w.text
            .extend_from_slice(unsafe { core::slice::from_raw_parts(data, len as usize) });
    }
}

/// Append a string as a JSON string literal (ECMAScript `QuoteJSONString`).
///
/// # Safety
/// `string` must be a string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_json_quote(w: *mut u8, string: *mut u8) {
    let w = writer(w);
    if w.failed {
        return;
    }
    // SAFETY: guaranteed by the caller.
    quote(&mut w.text, unsafe { crate::runtime::bytes(string) });
}

pub(crate) fn quote(text: &mut Buffer, bytes: &[u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    text.push(b'"');
    let mut start = 0;
    for (index, &byte) in bytes.iter().enumerate() {
        let escape: &[u8] = match byte {
            b'"' => br#"\""#,
            b'\\' => br"\\",
            0x08 => br"\b",
            0x0C => br"\f",
            b'\n' => br"\n",
            b'\r' => br"\r",
            b'\t' => br"\t",
            0..=0x1F => &[],
            _ => continue,
        };
        text.extend_from_slice(&bytes[start..index]);
        if escape.is_empty() {
            text.extend_from_slice(br"\u00");
            text.push(HEX[usize::from(byte >> 4)]);
            text.push(HEX[usize::from(byte & 15)]);
        } else {
            text.extend_from_slice(escape);
        }
        start = index + 1;
    }
    text.extend_from_slice(&bytes[start..]);
    text.push(b'"');
}

/// The text written, or null after a failure (with a `TypeError` raised).
#[unsafe(no_mangle)]
pub extern "C" fn std_json_finish(w: *mut u8) -> *mut u8 {
    let w = writer(w);
    if w.failed {
        raise(TYPE_ERROR, "Converting circular structure to JSON");
        return from_static("");
    }
    crate::runtime::from_bytes(w.text.as_slice())
}

/// `typeof value`.
#[unsafe(no_mangle)]
pub extern "C" fn std_json_type_of(value: *mut u8) -> *mut u8 {
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

fn value_text(text: &mut Buffer, value: *mut u8, depth: u32) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match word(value, 0) {
        NULL => text.extend_from_slice(b"null"),
        BOOLEAN => text.extend_from_slice(if word(value, 8) != 0 {
            b"true"
        } else {
            b"false"
        }),
        NUMBER => {
            // SAFETY: number nodes hold an f64 at 8.
            let number = unsafe { *(value.add(8) as *const f64) };
            if number.is_finite() {
                let mut digits = [0u8; 40];
                let len = crate::runtime::number_format(number, &mut digits);
                text.extend_from_slice(&digits[..len]);
            } else {
                text.extend_from_slice(b"null");
            }
        }
        STRING => quote(text, string_bytes(value)),
        kind @ (ARRAY | OBJECT) => {
            text.push(if kind == ARRAY { b'[' } else { b'{' });
            let mut child = word(value, 16);
            let mut first = true;
            while child != 0 {
                if !first {
                    text.push(b',');
                }
                first = false;
                if kind == OBJECT {
                    quote(text, string_bytes(node(word(node(child), 24))));
                    text.push(b':');
                }
                if !value_text(text, node(child), depth + 1) {
                    return false;
                }
                child = word(node(child), 4);
            }
            text.push(if kind == ARRAY { b']' } else { b'}' });
        }
        _ => return false,
    }
    true
}

/// Append a parsed JSON value (`JSON.stringify` of `JSON.parse` output; `undefined` writes
/// nothing, so callers check it first).
#[unsafe(no_mangle)]
pub extern "C" fn std_json_value(w: *mut u8, value: *mut u8) {
    let w = writer(w);
    if w.failed || value.is_null() {
        return;
    }
    if !value_text(&mut w.text, value, w.depth) {
        w.failed = true;
    }
}
