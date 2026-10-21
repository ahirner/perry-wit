//! Streaming `JSON.stringify` writer for typed values and retained JSON graphs.
use super::{canonical, json_number, key_bytes, native_snapshot, node, string_bytes, word};
use crate::runtime::{Buffer, TYPE_ERROR, adopt, from_static, raise};
use perry_abi::json::{MAX_DEPTH, kind::*};
use perry_abi::{
    layout::numeric::Tag,
    number::{Number, format_integer},
};

/// The writer of `JSON.stringify` for typed values: compiled serializers call it with
/// structure and pre-formatted pieces.
#[repr(C)]
struct Writer {
    text: Buffer,
    /// Bit `d` set: the container at depth `d` already has an element.
    started: [u64; 2],
    depth: u32,
    failed: bool,
}

fn writer(w: *mut u8) -> &'static mut Writer {
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
            failed: false,
        })
    };
    w as *mut u8
}

/// Enter a container value, returning `0` and marking the writer failed when nesting exceeds [MAX_DEPTH].
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_open(w: *mut u8, bracket: u32) -> u32 {
    let w = writer(w);
    if w.failed || w.depth >= MAX_DEPTH {
        w.failed = true;
        return 0;
    }
    w.text.push(bracket as u8);
    w.depth += 1;
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

/// Start an object member, where `key` is the pre-quoted key followed by `:`.
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
        append_number(&mut w.text, Number::Float(number));
    }
}

/// Serialize the compiler's numeric carriers without losing exact integer digits.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_write_numeric(w: *mut u8, tag: i32, bits: u64) {
    let w = writer(w);
    if !w.failed {
        // Generated serializers handle absence before calling the numeric writer.
        let tag = Tag::try_from(tag).expect("invalid JSON numeric tag");
        assert!(
            !matches!(tag, Tag::Null | Tag::Undefined | Tag::Omitted),
            "absent JSON number"
        );
        append_number(&mut w.text, Number::from_numeric(tag, bits));
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

pub(super) fn append_number(text: &mut Buffer, number: Number) {
    let (magnitude, negative) = match number {
        Number::Float(number) => {
            if number.is_finite() {
                let mut digits = [0; 40];
                let len = crate::runtime::number_format(number, &mut digits);
                text.extend_from_slice(&digits[..len]);
            } else {
                text.extend_from_slice(b"null");
            }
            return;
        }
        Number::Signed(value) => (value.unsigned_abs(), value < 0),
        Number::Unsigned(value) => (value, false),
    };
    let mut digits = [0; 66];
    text.extend_from_slice(format_integer(magnitude, negative, 10, &mut digits));
}

/// Append a JSON string literal, quotes included.
pub(super) fn quote(text: &mut Buffer, bytes: &[u8]) {
    text.push(b'"');
    perry_abi::json::quote_contents(bytes, |part| text.extend_from_slice(part));
    text.push(b'"');
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

/// Write a parsed JSON value starting `depth` levels deep, returning `false` when nesting
/// exceeds [MAX_DEPTH]. Uses an iterative traversal with a fixed stack of open containers so
/// the linker can bound the call stack of programs that serialize JSON values.
fn value_text(text: &mut Buffer, value: *mut u8, depth: u32) -> bool {
    /// An array or object being written: its node, its next member, and whether a member
    /// was written (for separators).
    #[derive(Clone, Copy)]
    struct Open {
        node: *mut u8,
        next: u32,
        written: bool,
    }
    let mut stack = arrayvec::ArrayVec::<Open, { MAX_DEPTH as usize + 1 }>::new();
    let mut current = value;
    unsafe {
        loop {
            if depth + stack.len() as u32 > MAX_DEPTH {
                return false;
            }
            if let Some(number) = json_number(current) {
                append_number(text, number);
            } else {
                match word(current, 0) {
                    NATIVE | VIEW => {
                        current = if word(current, 0) == VIEW {
                            super::view::std_json_snapshot(super::view::to_json(current))
                        } else {
                            native_snapshot(current)
                        };
                        continue;
                    }
                    BIGINT => {
                        raise(TYPE_ERROR, "Do not know how to serialize a BigInt");
                        return true;
                    }
                    EXCEPTION => text.extend_from_slice(b"{}"),
                    NULL | UNDEFINED => text.extend_from_slice(b"null"),
                    BOOLEAN => text.extend_from_slice(if word(current, 8) != 0 {
                        b"true"
                    } else {
                        b"false"
                    }),
                    STRING => quote(text, string_bytes(current)),
                    kind @ (ARRAY | OBJECT) => {
                        if kind == OBJECT {
                            current = canonical(current);
                        }
                        text.push(if kind == ARRAY { b'[' } else { b'{' });
                        stack.push(Open {
                            node: current,
                            next: word(current, 16),
                            written: false,
                        });
                    }
                    _ => return false,
                }
            }
            // The next member to write, closing the containers that are done.
            loop {
                let Some(container) = stack.last_mut() else {
                    return true;
                };
                let object = word(container.node, 0) == OBJECT;
                let mut member = container.next;
                while object && member != 0 && word(node(member), 0) == UNDEFINED {
                    member = word(node(member), 4);
                }
                if member == 0 {
                    text.push(if object { b'}' } else { b']' });
                    stack.pop();
                    continue;
                }
                if container.written {
                    text.push(b',');
                }
                if object {
                    quote(text, string_bytes(node(word(node(member), 24))));
                    text.push(b':');
                }
                container.next = word(node(member), 4);
                container.written = true;
                current = node(member);
                break;
            }
        }
    }
}

/// Append a parsed JSON value (`JSON.stringify` of `JSON.parse` output). A null address
/// (`undefined`) writes `null` in array element position, while callers omit it for object
/// members and at the top level.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_value(w: *mut u8, value: *mut u8) {
    let w = writer(w);
    if w.failed {
        return;
    }
    if value.is_null() {
        w.text.extend_from_slice(b"null");
    } else if !value_text(&mut w.text, value, w.depth) {
        w.failed = true;
    }
}

/// Write an undeclared property at its position among the native fields.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_write_member(w: *mut u8, member: *mut u8) {
    let w = writer(w);
    if w.failed || unsafe { word(member, 0) } == UNDEFINED {
        return;
    }
    separate(w);
    quote(&mut w.text, key_bytes(member as usize as u32));
    w.text.push(b':');
    if !value_text(&mut w.text, member, w.depth) {
        w.failed = true;
    }
}

/// Append undeclared object members or a tuple's remaining elements from its retained graph.
/// Declared values were already written from their current typed storage.
///
/// # Safety
/// `keys` holds `count` static string addresses sorted by byte length, then UTF-8 bytes.
/// Arrays do not read it.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_extras(
    w: *mut u8,
    graph: *mut u8,
    keys: *const u32,
    count: u32,
) {
    let w = writer(w);
    if graph.is_null() || w.failed {
        return;
    }
    unsafe {
        let object = word(graph, 0) == OBJECT;
        let graph = if object { canonical(graph) } else { graph };
        // SAFETY: compiled serializers supply the declared object's static field names.
        let keys = if object && count != 0 {
            core::slice::from_raw_parts(keys, count as usize)
        } else {
            &[]
        };
        let mut member = word(graph, 16);
        let mut index = if object { 0 } else { word(graph, 28) };
        while member != 0 {
            let value = node(member);
            let keep = if object {
                let name = key_bytes(member);
                keys.binary_search_by(|&key| {
                    // SAFETY: every table entry is a static string header.
                    perry_abi::json::key_cmp(crate::runtime::bytes(node(key)), name)
                })
                .is_err()
                    && word(value, 0) != UNDEFINED
            } else {
                index >= count
            };
            if keep {
                separate(w);
                if object {
                    quote(&mut w.text, key_bytes(member));
                    w.text.push(b':');
                }
                if !value_text(&mut w.text, value, w.depth) {
                    w.failed = true;
                    return;
                }
            }
            index += 1;
            member = word(value, 4);
        }
    }
}
