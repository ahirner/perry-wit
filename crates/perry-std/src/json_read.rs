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
use crate::runtime::{
    ERROR, TYPE_ERROR, adopt, alloc_shaped, alloc_uninit, bytes, from_static,
    layout::string::Header as StringHeader, raise, string_view_into,
};
use core::ptr;

#[path = "../../../src/helpers/json/pull.rs"]
pub(super) mod pull;

/// Value kinds of [std_json_read_kind], as the graph parser's node kinds.
const NULL: u32 = 1;
const BOOLEAN: u32 = 2;
const NUMBER: u32 = 3;
const STRING: u32 = 4;
const ARRAY: u32 = 5;
const OBJECT: u32 = 6;
/// Mismatch of a variant tag that names no declared case.
const VARIANT_TAG: u32 = 7;

/// Type errors by expected kind (or [VARIANT_TAG]), as the graph decoders raise them.
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

/// The state of one `JSON.parse`. `text` is traced: it keeps the cursor's input alive,
/// and with them every string view the reader hands out.
#[repr(C)]
pub(super) struct Reader {
    text: *mut u8,
    pub(super) cursor: pull::Cursor<'static>,
    /// The first deferred mismatch (an index of [MISMATCH]), or 0.
    pub(super) mismatch: u32,
    /// A decoded aggregate was written to, so retained sources may no longer match.
    pub(super) dirty: bool,
    /// False once a subtree escaped to the graph parser; source copies are then unsafe.
    pub(super) copy_safe: bool,
    /// Offset of the current variant's `val` member, or `u32::MAX`.
    payload: u32,
    /// Chunk of 64 `json_source::Source` slots, `source_used` of them taken.
    pub(super) sources: *mut u8,
    pub(super) source_used: u32,
    /// Chunk of 64 string view headers, `string_used` of them taken.
    strings: *mut u8,
    string_used: u32,
    /// Bytes of re-serialized text cached by sources, bounded by the input length.
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

/// The reader behind the handle compiled decoders pass, as every helper ABI handle (see
/// `json::writer`): the one place this module trusts a reader address.
pub(super) fn reader(r: *mut u8) -> &'static mut Reader {
    // SAFETY: compiled decoders pass the reader that `std_json_read_begin` returned; it lives
    // through the task and no other reference to it is held across this call.
    unsafe { &mut *(r as *mut Reader) }
}

/// Slots per pooled chunk of [pooled_slot].
const POOL: usize = 64;

/// The next zeroed `size`-byte slot of a pool: `chunk` holds [POOL] slots, `used` of them
/// handed out. A used-up pool continues in a fresh chunk traced by `shape`.
///
/// # Safety
/// `shape` is the pointer map of one `size`-byte slot, repeated (`[size, …]`); `chunk` and
/// `used` form a pool that only this function fills, starting with `used == POOL`.
unsafe fn pooled_slot(
    chunk: &mut *mut u8,
    used: &mut u32,
    size: usize,
    shape: &'static [u32],
) -> *mut u8 {
    if *used as usize == POOL {
        // SAFETY: guaranteed by the caller; the chunk is zeroed before anything traces it.
        unsafe {
            *chunk = alloc_shaped((size * POOL) as u32, shape);
            ptr::write_bytes(*chunk, 0, size * POOL);
        }
        *used = 0;
    }
    // SAFETY: `used < POOL`, so the slot lies within the chunk.
    let slot = unsafe { chunk.add(*used as usize * size) };
    *used += 1;
    slot
}

/// A reader over `text`, positioned at its root value.
///
/// # Safety
/// `text` must be a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_begin(text: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller; the reader traces `text`, keeping its bytes alive.
    let input = unsafe { bytes(text) };
    let r = crate::runtime::object!(Reader; text, sources, strings);
    // SAFETY: a fresh allocation of a `Reader`, whose shape lists its pointer fields.
    unsafe {
        r.write(Reader {
            text,
            cursor: pull::Cursor::new(input),
            mismatch: 0,
            dirty: false,
            copy_safe: true,
            payload: u32::MAX,
            sources: ptr::null_mut(),
            source_used: POOL as u32,
            strings: ptr::null_mut(),
            string_used: POOL as u32,
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

/// Remove and return the deferred mismatch, so a decoder can scope it to one field value.
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

/// String content read from the input.
enum Text {
    /// Unescaped bytes `start..end` of the input, holding `scalars` Unicode scalars.
    View { start: u32, end: u32, scalars: u32 },
    /// Decoded escapes in task storage.
    Decoded(&'static [u8]),
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
    match r.text() {
        Some(Text::View {
            start,
            end,
            scalars,
        }) => r.view(start, end, scalars),
        // SAFETY: decoded UTF-8 in a fresh task allocation, which the string now owns.
        Some(Text::Decoded(decoded)) => unsafe {
            adopt(decoded.as_ptr().cast_mut(), decoded.len() as u32)
        },
        None => from_static(""),
    }
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
    reader(r).element()
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
    // SAFETY: guaranteed by the caller.
    let table = unsafe { bytes(table) };
    while let Some((key, escaped)) = r.member() {
        let found = lookup(table, hint, if escaped { unescape(key) } else { key });
        if found >= 0 {
            return found;
        }
        r.skip();
    }
    -1
}

/// The index of `key` in a key table (see [std_json_read_field]), or -1.
pub(super) fn lookup(table: &[u8], hint: u32, key: &[u8]) -> i32 {
    let count = table_number(table, 0);
    // Compare lengths first: they are in the header, the key bytes are not.
    let matches = |index: u32| {
        table_number(table, 5 + 10 * index as usize) as usize == key.len()
            && table_key(table, index) == key
    };
    if hint < count && matches(hint) {
        return hint as i32;
    }
    (0..count)
        .find(|&index| index != hint && matches(index))
        .map_or(-1, |index| index as i32)
}

/// The five base-128 digits at `at` (least significant first).
///
/// Key tables are static strings the compiler generates (`perry-lower`'s `encode_keys`); a
/// short one is a compiler bug, so it traps instead of reading as some other table.
fn table_number(table: &[u8], at: usize) -> u32 {
    table
        .get(at..)
        .and_then(<[u8]>::first_chunk::<5>)
        .expect("generated key tables hold every header number")
        .iter()
        .rev()
        .fold(0, |value, &digit| (value << 7) | u32::from(digit))
}

/// The key bytes of entry `index`, which must be below the table's count.
pub(super) fn table_key(table: &[u8], index: u32) -> &[u8] {
    let at = 5 + 10 * index as usize;
    let len = table_number(table, at) as usize;
    let offset = table_number(table, at + 5) as usize;
    table
        .get(offset..offset + len)
        .expect("generated key table entries lie within the table")
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

    /// The next value's string content; a mismatch records an error. `None` when the value
    /// is not a string or the text is broken.
    fn text(&mut self) -> Option<Text> {
        if self.broken != 0 {
            return None;
        }
        if self.whitespace() != b'"' {
            self.mismatched(STRING);
            return None;
        }
        self.pos += 1;
        let start = self.pos;
        let (escaped, continuations) = self.string();
        if self.broken != 0 {
            return None;
        }
        let end = self.pos - 1;
        Some(if escaped {
            Text::Decoded(unescape(&self.input[start as usize..end as usize]))
        } else {
            Text::View {
                start,
                end,
                scalars: end - start - continuations,
            }
        })
    }

    /// A string over input bytes `start..end` with `scalars` scalars, in a pooled header.
    fn view(&mut self, start: u32, end: u32, scalars: u32) -> *mut u8 {
        const HEADER: usize = core::mem::size_of::<StringHeader>();
        static SHAPE: [u32; 3] = [
            HEADER as u32,
            1,
            core::mem::offset_of!(StringHeader, owner) as u32,
        ];
        // SAFETY: `SHAPE` maps a header's owner; only this method fills the string pool.
        let view = unsafe { pooled_slot(&mut self.strings, &mut self.string_used, HEADER, &SHAPE) };
        // SAFETY: a zeroed, traced header slot; `start..end` is validated UTF-8 between the
        // quotes of the input, which `text` owns, and holds `scalars` scalars.
        unsafe { string_view_into(view, self.text, start, end, scalars) };
        view
    }

    fn raise_recorded(&self) {
        if self.broken != 0 {
            raise(ERROR, "SyntaxError: Unexpected token in JSON");
        } else if self.mismatch != 0 {
            raise(TYPE_ERROR, MISMATCH[self.mismatch as usize]);
        }
    }
}

/// Decode a validated escaped string into task storage for a caller that retains its bytes
/// (through a string owner or a traced key entry).
pub(super) fn unescape(input: &[u8]) -> &'static [u8] {
    // Escapes only shrink: six bytes encode at most three, a surrogate pair twelve to four.
    let out = alloc_uninit(input.len());
    let mut written = 0;
    decode_string(input, |bytes| {
        out[written..written + bytes.len()].write_copy_of_slice(bytes);
        written += bytes.len();
    });
    let out: &'static [core::mem::MaybeUninit<u8>] = out;
    // SAFETY: the decoder initialized the first `written` bytes.
    unsafe { out[..written].assume_init_ref() }
}

/// Emit borrowed runs and decoded escapes without constructing a temporary string.
/// `input` is the validated content between JSON quotes.
pub(super) fn decode_string(input: &[u8], mut emit: impl FnMut(&[u8])) {
    let mut rest = input;
    loop {
        let run = rest
            .iter()
            .position(|&byte| byte == b'\\')
            .unwrap_or(rest.len());
        let (literal, escape) = rest.split_at(run);
        if !literal.is_empty() {
            emit(literal);
        }
        let [b'\\', kind, after @ ..] = escape else {
            return;
        };
        rest = after;
        let scalar = match kind {
            b'b' => '\u{8}',
            b'f' => '\u{c}',
            b'n' => '\n',
            b'r' => '\r',
            b't' => '\t',
            b'u' => {
                let (scalar, after) = unicode_escape(after);
                rest = after;
                scalar
            }
            other => char::from(*other),
        };
        emit(scalar.encode_utf8(&mut [0; 4]).as_bytes());
    }
}

/// The scalar of a validated `\u` escape from its hex digits on (a high surrogate spans the
/// following low one), and the input after it.
///
/// The cursor admits only four hex digits and paired surrogates, so a failure here is a
/// validation bug and traps rather than decoding something else.
fn unicode_escape(input: &[u8]) -> (char, &[u8]) {
    const VALIDATED: &str = "the cursor validated this \\u escape";
    let (high, rest) = pull::hex4(input).expect(VALIDATED);
    let (point, rest) = match (high, rest) {
        (0xD800..=0xDBFF, [b'\\', b'u', digits @ ..]) => {
            let (low, rest) = pull::hex4(digits).expect(VALIDATED);
            (0x10000 + ((high - 0xD800) << 10) + low - 0xDC00, rest)
        }
        _ => (high, rest),
    };
    (char::from_u32(point).expect(VALIDATED), rest)
}

/// The offset of the next value, which decoders keep to attach a source span.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_position(r: *mut u8) -> u32 {
    let r = reader(r);
    r.whitespace();
    r.pos
}

/// Validate the complete tagged object and remember its final payload span. Returns the
/// index of its `tag` in the case table, or -1 with a recorded mismatch.
///
/// # Safety
/// `table` must be a key table string (see [std_json_read_field]).
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
        let input = r.input;
        // A tag that is not a string matches no case.
        let value = match r.text() {
            Some(Text::View { start, end, .. }) => &input[start as usize..end as usize],
            Some(Text::Decoded(decoded)) => decoded,
            None => b"",
        };
        // SAFETY: guaranteed by the caller.
        lookup(unsafe { bytes(table) }, 0, value)
    } else {
        -1
    };
    r.pos = end;
    if tag < 0 {
        r.record(VARIANT_TAG);
    }
    tag
}

/// Position the reader at the payload of the variant [std_json_read_variant] validated,
/// within its object: 1 when it has one. Decoders read it, then call [std_json_read_resume].
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_payload(r: *mut u8) -> u32 {
    let r = reader(r);
    if r.payload == u32::MAX || r.broken != 0 {
        return 0;
    }
    r.pos = r.payload;
    u32::from(r.push(true))
}

/// Leave a variant payload, returning to `end`, the offset after its tagged object.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_json_read_resume(r: *mut u8, end: u32) {
    let r = reader(r);
    r.depth -= 1;
    r.pos = end;
}
