//! Single-pass pull reader behind typed `JSON.parse`.
//!
//! Lowering generates one decoder per declared target type that drives this reader directly:
//! no intermediate graph and no member search. Object keys resolve against a static key
//! table of the record ([std_json_read_field]); undeclared members are validated and skipped
//! inside the reader. Unescaped strings become views of the input text.
//!
//! Syntax errors take precedence over typed validation. Generated decoders retain a deferred
//! mismatch for each final field value, replacing it when a duplicate overwrites that field.
//! Once the text is broken, reads return defaults and containers end so decoders unwind.
//! [std_json_read_end] raises the remaining error after validating the complete document.
//!
//! The reader keeps its own stack of open containers (bits, at most [MAX_DEPTH]): skipping
//! is iterative, so the linker can bound the stack of programs that parse JSON.
use crate::runtime::{ERROR, TYPE_ERROR, adopt, alloc, from_static, raise, string_view_counted};

#[allow(unreachable_pub)]
#[path = "../../../src/helpers/json/pull.rs"]
pub(super) mod pull;

/// Value kinds of [std_json_read_kind], as the graph parser's node kinds.
const NULL: u32 = 1;
const BOOLEAN: u32 = 2;
const NUMBER: u32 = 3;
const STRING: u32 = 4;
const ARRAY: u32 = 5;
const OBJECT: u32 = 6;

/// Type errors by expected kind, as the graph decoders raise them.
const MISMATCH: [&str; 8] = [
    "",
    "JSON value is not null",
    "JSON value is not a boolean",
    "JSON value is not a number",
    "JSON value is not a string",
    "JSON value is not an array",
    "JSON value is not an object",
    "JSON tag is not a case of the declared variant",
];

/// The state of one `JSON.parse`. `text` is traced: it keeps the bytes behind `data` alive,
/// and with them every string view the reader hands out.
#[repr(C)]
pub(super) struct Reader {
    pub(super) text: *mut u8,
    pub(super) data: *const u8,
    pub(super) cursor: pull::Cursor<'static>,
    pub(super) mismatch: u32,
    pub(super) dirty: bool,
    pub(super) copy_safe: bool,
    pub(super) payload: u32,
    pub(super) sources: *mut u8,
    pub(super) source_used: u32,
    pub(super) strings: *mut u8,
    pub(super) string_used: u32,
    pub(super) pooled: bool,
    pub(super) cached_bytes: u32,
}
impl core::ops::Deref for Reader {
    type Target = pull::Cursor<'static>;
    fn deref(&self) -> &Self::Target {
        &self.cursor
    }
}
impl core::ops::DerefMut for Reader {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cursor
    }
}

pub(super) fn reader(r: *mut u8) -> &'static mut Reader {
    // SAFETY: compiled decoders pass the reader that `std_json_read_begin` returned.
    unsafe { &mut *(r as *mut Reader) }
}

/// A reader over `text`, positioned at its root value.
///
/// # Safety
/// `text` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_begin(text: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { crate::runtime::bytes(text) };
    let r = crate::runtime::object!(Reader; text, sources, strings);
    // SAFETY: a fresh allocation of a `Reader`.
    unsafe {
        r.write(Reader {
            text,
            data: bytes.as_ptr(),
            cursor: pull::Cursor::new(core::str::from_utf8_unchecked(bytes)),
            mismatch: 0,
            dirty: false,
            copy_safe: true,
            payload: u32::MAX,
            sources: core::ptr::null_mut(),
            source_used: 64,
            strings: core::ptr::null_mut(),
            string_used: 64,
            pooled: true,
            cached_bytes: 0,
        })
    };
    r as *mut u8
}

/// Finish a document: only whitespace may follow the root value. Raises a syntax error, or
/// else the first recorded type mismatch.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_end(r: *mut u8) {
    let r = reader(r);
    if r.broken == 0 && (r.whitespace() != 0 || r.pos < r.len) {
        r.broken = 1;
    }
    r.raise_recorded();
}

/// Defer a typed mismatch until the decoder finishes validating the document.
/// Generated record decoders can discard it if a later duplicate replaces the value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_fail(r: *mut u8, expected: u32) {
    reader(r).record(expected);
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_take_error(r: *mut u8) -> u32 {
    core::mem::take(&mut reader(r).mismatch)
}

/// The kind of the next value (`NULL` … `OBJECT`), or 0 when it cannot start one.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_kind(r: *mut u8) -> u32 {
    let r = reader(r);
    if r.broken != 0 {
        return 0;
    }
    match r.whitespace() {
        b'n' => NULL,
        b't' | b'f' => BOOLEAN,
        b'"' => STRING,
        b'[' => ARRAY,
        b'{' => OBJECT,
        b'-' | b'0'..=b'9' => NUMBER,
        _ => 0,
    }
}

/// Skip (and validate) the next value.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_skip(r: *mut u8) {
    reader(r).skip();
}

/// The validated text of a dynamic JSON subtree for decoding through the graph parser.
/// Returns empty text, which JSON.parse rejects, when the input is broken.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_raw(r: *mut u8) -> *mut u8 {
    let r = reader(r);
    r.copy_safe = false;
    r.whitespace();
    let start = r.pos;
    r.skip();
    if r.broken != 0 {
        return from_static("");
    }
    // SAFETY: a validated value of the input text, which `r.text` owns.
    unsafe { crate::runtime::string_view(r.text, start, r.pos) }
}

/// The next value as a number; a mismatch records an error and reads 0.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_number(r: *mut u8) -> f64 {
    let r = reader(r);
    if r.broken != 0 {
        return 0.0;
    }
    match r.whitespace() {
        b'-' | b'0'..=b'9' => r.number(),
        _ => {
            r.mismatched(NUMBER);
            0.0
        }
    }
}

/// The next value as a boolean; a mismatch records an error and reads `false`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_bool(r: *mut u8) -> u32 {
    let r = reader(r);
    if r.broken != 0 {
        return 0;
    }
    match r.whitespace() {
        b't' => u32::from(r.literal(b"true")),
        b'f' => {
            r.literal(b"false");
            0
        }
        _ => {
            r.mismatched(BOOLEAN);
            0
        }
    }
}

/// The next value as a string: a view of the input unless it has escapes. A mismatch records
/// an error and reads `""`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_string(r: *mut u8) -> *mut u8 {
    let r = reader(r);
    if r.broken == 0 && r.whitespace() == b'"' {
        r.pos += 1;
        let start = r.pos;
        let (escaped, continuations) = r.string();
        if r.broken == 0 {
            let end = r.pos - 1;
            let len = end - start;
            return if escaped {
                r.unescape(start, end)
            } else {
                // SAFETY: valid UTF-8 between quotes of the input text, which `r.text` owns.
                unsafe {
                    if r.pooled {
                        const HEADER: usize =
                            core::mem::size_of::<crate::runtime::layout::string::Header>();
                        static SHAPE: [u32; 3] = [
                            HEADER as u32,
                            1,
                            core::mem::offset_of!(crate::runtime::layout::string::Header, owner)
                                as u32,
                        ];
                        if r.string_used == 64 {
                            r.strings = crate::runtime::alloc_shaped((HEADER * 64) as u32, &SHAPE);
                            core::ptr::write_bytes(r.strings, 0, HEADER * 64);
                            r.string_used = 0;
                        }
                        let view = r.strings.add(r.string_used as usize * HEADER);
                        r.string_used += 1;
                        crate::runtime::string_view_into(
                            view,
                            r.text,
                            start,
                            end,
                            len - continuations,
                        );
                        view
                    } else {
                        string_view_counted(r.text, start, end, len - continuations)
                    }
                }
            };
        }
    } else if r.broken == 0 {
        r.mismatched(STRING);
    }
    from_static("")
}

/// Enter an object: true when the next value is one. Otherwise it is skipped and the
/// decoder calls [std_json_read_fail].
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_object(r: *mut u8) -> u32 {
    reader(r).open(b'{')
}

/// Enter an array, as [std_json_read_object].
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_array(r: *mut u8) -> u32 {
    reader(r).open(b'[')
}

/// Advance to the next element of the innermost array: true when one follows, false when
/// the array ended (or the text is broken).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_element(r: *mut u8) -> u32 {
    let r = reader(r);
    if r.broken != 0 {
        return 0;
    }
    let next = r.whitespace();
    if core::mem::take(&mut r.fresh) != 0 {
        if next == b']' {
            r.close();
            return 0;
        }
        return 1;
    }
    match next {
        b',' => {
            r.pos += 1;
            1
        }
        b']' => {
            r.close();
            0
        }
        _ => {
            r.broken = 1;
            0
        }
    }
}

/// Advance to the next declared member of the innermost object: the index of its key in
/// `table`, positioned at its value, or -1 when the object ended (or the text is broken).
/// Undeclared members are validated and skipped. `hint` is the index expected next (members
/// usually come in declaration order); it is tried first.
///
/// `table` is a static string: a count followed by length/offset pairs and UTF-8 key bytes.
/// Each integer occupies five base-128 digits, preserving valid UTF-8 for the table header.
///
/// # Safety
/// `table` must be a string in that format.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_field(r: *mut u8, table: *mut u8, hint: u32) -> i32 {
    let r = reader(r);
    if r.broken != 0 {
        return -1;
    }
    // SAFETY: guaranteed by the caller.
    let table = unsafe { crate::runtime::bytes(table) };
    let mut next = r.whitespace();
    if core::mem::take(&mut r.fresh) == 0 {
        match next {
            b',' => {
                r.pos += 1;
                next = r.whitespace();
            }
            b'}' => {
                r.close();
                return -1;
            }
            _ => {
                r.broken = 1;
                return -1;
            }
        }
    } else if next == b'}' {
        r.close();
        return -1;
    }
    loop {
        if next != b'"' {
            r.broken = 1;
            return -1;
        }
        r.pos += 1;
        let start = r.pos;
        let (escaped, _) = r.string();
        let end = r.pos - 1;
        if r.whitespace() != b':' || r.broken != 0 {
            r.broken = 1;
            return -1;
        }
        r.pos += 1;
        let found = if escaped {
            // Keys with escapes are rare: compare their decoded text.
            let key = r.unescape(start, end);
            // SAFETY: `unescape` returned a string.
            lookup(table, hint, unsafe { crate::runtime::bytes(key) })
        } else {
            // SAFETY: the key lies within the input text.
            let key = unsafe {
                core::slice::from_raw_parts(r.data.add(start as usize), (end - start) as usize)
            };
            lookup(table, hint, key)
        };
        if found >= 0 {
            return found;
        }
        r.skip();
        match r.whitespace() {
            b',' => {
                r.pos += 1;
                next = r.whitespace();
            }
            b'}' => {
                r.close();
                return -1;
            }
            _ => {
                r.broken = 1;
                return -1;
            }
        }
    }
}

/// The index of `key` in a key table (see [std_json_read_field]), or -1.
pub(super) fn lookup(table: &[u8], hint: u32, key: &[u8]) -> i32 {
    let count = table_number(table, 0);
    let entry = |index: u32| table_key(table, index);
    if hint < count && entry(hint) == key {
        return hint as i32;
    }
    let mut index = 0;
    while index < count {
        if table_number(table, 5 + 10 * index as usize) as usize == key.len()
            && index != hint
            && entry(index) == key
        {
            return index as i32;
        }
        index += 1;
    }
    -1
}

pub(super) fn table_number(table: &[u8], at: usize) -> u32 {
    let digits = &table[at..at + 5];
    u32::from(digits[0])
        | (u32::from(digits[1]) << 7)
        | (u32::from(digits[2]) << 14)
        | (u32::from(digits[3]) << 21)
        | (u32::from(digits[4]) << 28)
}

pub(super) fn table_key(table: &[u8], index: u32) -> &[u8] {
    let at = 5 + 10 * index as usize;
    let len = table_number(table, at) as usize;
    let offset = table_number(table, at + 5) as usize;
    &table[offset..offset + len]
}

impl Reader {
    fn record(&mut self, expected: u32) {
        if self.mismatch == 0 {
            self.mismatch = expected;
        }
    }

    fn mismatched(&mut self, expected: u32) {
        self.record(expected);
        self.skip();
    }

    fn raise_recorded(&self) {
        if self.broken != 0 {
            raise(ERROR, "SyntaxError: Unexpected token in JSON");
        } else if self.mismatch != 0 {
            raise(TYPE_ERROR, MISMATCH[self.mismatch as usize]);
        }
    }

    fn unescape(&mut self, start: u32, end: u32) -> *mut u8 {
        // Escapes only shrink: `\uXXXX` (6 bytes) encodes at most 3, a surrogate pair (12) 4.
        let capacity = end - start;
        let out = alloc(capacity.max(1));
        let mut written = 0usize;
        let mut at = start;
        let put = |bytes: &[u8], written: &mut usize| {
            // SAFETY: decoded text never exceeds the escaped text's length.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), out.add(*written), bytes.len())
            };
            *written += bytes.len();
        };
        while at < end {
            let byte = self.byte(at);
            if byte != b'\\' {
                let run_end = (at..end).find(|&i| self.byte(i) == b'\\').unwrap_or(end);
                // SAFETY: within the input text.
                let run = unsafe {
                    core::slice::from_raw_parts(self.data.add(at as usize), (run_end - at) as usize)
                };
                put(run, &mut written);
                at = run_end;
                continue;
            }
            let scalar = match self.byte(at + 1) {
                b'b' => '\u{8}',
                b'f' => '\u{c}',
                b'n' => '\n',
                b'r' => '\r',
                b't' => '\t',
                b'u' => {
                    let quad = |from: u32| {
                        (from..from + 4).fold(0, |value, i| {
                            value * 16 + (self.byte(i) as char).to_digit(16).unwrap_or(0)
                        })
                    };
                    let high = quad(at + 2);
                    let point = if (0xD800..=0xDBFF).contains(&high) {
                        let low = quad(at + 8);
                        at += 6;
                        0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
                    } else {
                        high
                    };
                    at += 4;
                    char::from_u32(point).unwrap_or('\u{FFFD}')
                }
                other => other as char,
            };
            at += 2;
            let mut buffer = [0; 4];
            put(scalar.encode_utf8(&mut buffer).as_bytes(), &mut written);
        }
        // SAFETY: `written` bytes of valid UTF-8 at `out`, a task-region allocation.
        unsafe { adopt(out, written as u32) }
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_position(r: *mut u8) -> u32 {
    let r = reader(r);
    r.whitespace();
    r.pos
}

/// Validate the complete tagged object and remember its final payload span.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_variant(r: *mut u8, table: *mut u8) -> i32 {
    let r = reader(r);
    if r.open(b'{') == 0 {
        r.record(OBJECT);
        return -1;
    }
    let mut tag = None;
    r.payload = u32::MAX;
    while let Some(member) = crate::json_source::next_member(r) {
        match crate::json_source::key_bytes(&member) {
            b"tag" => tag = Some(member.start as u32),
            b"val" => r.payload = member.start as u32,
            _ => {}
        }
    }
    if r.broken != 0 {
        return -1;
    }
    let end = r.pos;
    let tag = if let Some(tag) = tag {
        r.pos = tag;
        let value = std_json_read_string((r as *mut Reader).cast());
        lookup(unsafe { crate::runtime::bytes(table) }, 0, unsafe {
            crate::runtime::bytes(value)
        })
    } else {
        -1
    };
    r.pos = end;
    if tag < 0 {
        r.record(7);
    }
    tag
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_payload(r: *mut u8) -> u32 {
    let r = reader(r);
    if r.payload == u32::MAX || r.broken != 0 {
        return 0;
    }
    r.pos = r.payload;
    u32::from(r.push(true))
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_resume(r: *mut u8, end: u32) {
    let r = reader(r);
    r.depth -= 1;
    r.pos = end;
}
