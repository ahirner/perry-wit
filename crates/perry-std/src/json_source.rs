//! Source spans retained by typed JSON aggregates and consumed by their serializers.
//!
//! A decoded record, array or variant keeps the span of text it came from. `JSON.stringify`
//! of it writes the members its type does not declare straight from that span, and repeats a
//! subtree's earlier serialization while nothing in the document changed.
use crate::json::{
    MAX_DEPTH, array_index, quote, quote_contents, std_json_close, std_json_item, std_json_number,
    std_json_open, writer as json_writer,
};
use crate::json_read::{Reader, decode_string, lookup, pull, reader, table_key, unescape};
use crate::runtime::{Buffer, bytes};
use arrayvec::ArrayVec;
use core::{mem, ptr, slice};
use perry_abi::layout::json_source;

/// The text span behind one decoded aggregate, and what the program changed since.
///
/// Only [std_json_read_attach] creates sources, from a reader and a key table string, so the
/// accessors below may rely on those fields.
#[repr(C)]
struct Source {
    document: *mut Reader,
    table: *mut u8,
    start: u32,
    end: u32,
    assigned: Buffer,
    cached: *const u8,
    cached_len: u32,
    nesting: u32,
    tuple_written: u32,
    tuple: bool,
}

impl Source {
    /// The reader whose input this source spans.
    fn document(&self) -> &'static mut Reader {
        reader(self.document.cast())
    }

    /// The record's key table (see `std_json_read_field`), empty for arrays and tuples.
    fn table(&self) -> &'static [u8] {
        // SAFETY: attached sources hold a key table string, which they trace.
        unsafe { bytes(self.table) }
    }

    /// The remembered serialization of [std_json_source_remember], if any.
    fn cached(&self) -> Option<&'static [u8]> {
        // SAFETY: `cached` is null or the start of `cached_len` immutable bytes of an
        // allocation this source traces.
        (!self.cached.is_null())
            .then(|| unsafe { slice::from_raw_parts(self.cached, self.cached_len as usize) })
    }
}

/// The attached source behind a handle compiled code passes, if any.
///
/// # Safety
/// `source` is null or an attached source that no other reference borrows.
unsafe fn attached<'a>(source: *mut u8) -> Option<&'a mut Source> {
    // SAFETY: guaranteed by the caller.
    unsafe { source.cast::<Source>().as_mut() }
}

/// One object member of a source span: its (decoded) key and where its value starts.
///
/// `repr(C)` of pointer-sized words: the key pointer leads (an [Entry] buffer traces it) and
/// no padding leaves uninitialized bytes, so [entry_bytes] may view entries as bytes.
#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Entry {
    key: *const u8,
    key_len: usize,
    pub(super) start: usize,
    /// Index of the declared field the key names, or `usize::MAX`.
    field: usize,
    /// Position in serialization order before sorting.
    order: usize,
}

const _: () = assert!(mem::size_of::<Entry>() == 5 * mem::size_of::<usize>());
// Task allocations are 8-aligned, so entry buffers are aligned for `Entry`.
const _: () = assert!(mem::align_of::<Entry>() <= 8);

impl Entry {
    /// A member named `key`, which outlives every use of the entry: the source text, a
    /// decoded key or a key table, each retained through the document.
    fn new(key: &'static [u8], start: usize, field: usize, order: usize) -> Self {
        Self {
            key: key.as_ptr(),
            key_len: key.len(),
            start,
            field,
            order,
        }
    }
}

/// The deduplicated members of a source object in serialization order, and a cursor.
#[repr(C)]
struct Members {
    document: *mut Reader,
    /// Complete [Entry] values only; the first `count` are the members.
    entries: Buffer,
    at: usize,
    count: usize,
}

fn cursor(document: *mut Reader, start: u32) -> pull::Cursor<'static> {
    reader(document.cast()).cursor.at(start)
}

pub(super) fn key_bytes(entry: &Entry) -> &'static [u8] {
    // SAFETY: entries are made by [Entry::new] (or are the empty placeholder) from a key
    // slice that outlives them.
    if entry.key_len == 0 {
        &[]
    } else {
        unsafe { slice::from_raw_parts(entry.key, entry.key_len) }
    }
}

/// `entries` as bytes, for storing them in a traced buffer.
fn entry_bytes(entries: &[Entry]) -> &[u8] {
    // SAFETY: entries are padding-free words (see [Entry]), so every byte is initialized.
    unsafe { slice::from_raw_parts(entries.as_ptr().cast(), mem::size_of_val(entries)) }
}

/// The complete entries stored in `buffer`.
fn stored(buffer: &mut Buffer) -> &mut [Entry] {
    let bytes = buffer.as_mut_slice();
    if bytes.is_empty() {
        return &mut [];
    }
    // SAFETY: entry buffers hold only whole entries written by [entry_bytes], in an 8-aligned
    // task allocation; the exclusive borrow covers exactly those bytes.
    unsafe {
        slice::from_raw_parts_mut(
            bytes.as_mut_ptr().cast(),
            bytes.len() / mem::size_of::<Entry>(),
        )
    }
}

fn entries(members: &mut Members) -> &mut [Entry] {
    let count = members.count;
    &mut stored(&mut members.entries)[..count]
}

fn append(members: &mut Members, entry: Entry) {
    let count = members.count;
    match stored(&mut members.entries).get_mut(count) {
        Some(slot) => *slot = entry,
        None => members.entries.extend_from_slice(entry_bytes(&[entry])),
    }
    members.count += 1;
}

pub(super) fn next_member(r: &mut pull::Cursor<'static>) -> Option<Entry> {
    let (key, escaped) = r.member()?;
    let key = if escaped { unescape(key) } else { key };
    r.whitespace();
    let start = r.pos;
    r.skip();
    Some(Entry::new(key, start as usize, usize::MAX, 0))
}

/// The deduplicated members of the source object at `start`; with a key `table`, each
/// entry notes the declared field it names.
fn index_object(document: *mut Reader, start: u32, table: Option<&[u8]>) -> Members {
    static SHAPE: [u32; 3] = [mem::size_of::<Entry>() as u32, 1, 0];
    let mut members = Members {
        document,
        // SAFETY: `SHAPE` maps an entry's leading key pointer, and the buffer holds entries.
        entries: unsafe { Buffer::with_shape(&SHAPE) },
        at: 0,
        count: 0,
    };
    let mut pending = ArrayVec::<Entry, 128>::new();
    let mut count = 0;
    let mut scan = cursor(document, start);
    scan.open(b'{');
    while let Some(mut entry) = next_member(&mut scan) {
        entry.order = count;
        if let Some(table) = table {
            entry.field = lookup(table, 0, key_bytes(&entry)) as usize;
        }
        if count < pending.capacity() {
            pending.push(entry);
        } else {
            if count == pending.len() {
                members.entries.extend_from_slice(entry_bytes(&pending));
                members.count = count;
            }
            append(&mut members, entry);
        }
        count += 1;
    }
    if count <= pending.capacity() {
        members.count = index_small_object(&mut pending);
        if members.count != 0 {
            members
                .entries
                .extend_from_slice(entry_bytes(&pending[..members.count]));
        }
        return members;
    }
    if members.count > 0 {
        let items = entries(&mut members);
        heap_sort(items, |a, b| {
            key_bytes(a).cmp(key_bytes(b)).then(a.order.cmp(&b.order))
        });
        let mut count = 0;
        for read in 0..items.len() {
            let mut entry = items[read];
            if count > 0 && key_bytes(&items[count - 1]) == key_bytes(&entry) {
                entry.order = items[count - 1].order;
                items[count - 1] = entry;
            } else {
                items[count] = entry;
                count += 1;
            }
        }
        members.count = count;
    }
    members
}

/// A fixed-size index bounds collisions and keeps small objects in insertion order.
fn index_small_object(items: &mut [Entry]) -> usize {
    let mut buckets = [0u8; 256];
    let mut count = 0;
    for read in 0..items.len() {
        let mut entry = items[read];
        let hash = key_bytes(&entry).iter().fold(2166136261u32, |hash, byte| {
            (hash ^ u32::from(*byte)).wrapping_mul(16777619)
        });
        let mut slot = hash as usize & (buckets.len() - 1);
        loop {
            let previous = buckets[slot];
            if previous == 0 {
                items[count] = entry;
                count += 1;
                buckets[slot] = count as u8;
                break;
            }
            let previous = usize::from(previous - 1);
            if key_bytes(&items[previous]) == key_bytes(&entry) {
                entry.order = items[previous].order;
                items[previous] = entry;
                break;
            }
            slot = (slot + 1) & (buckets.len() - 1);
        }
    }
    count
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

fn sort(members: &mut Members) {
    let count = entries(members).len();
    if count == 0 {
        return;
    }
    let entries = entries(members);
    let order = |entry: &Entry| match array_index(key_bytes(entry)) {
        Some(index) => (0, index as usize),
        None => (1, entry.order),
    };
    if !entries
        .windows(2)
        .all(|pair| order(&pair[0]) <= order(&pair[1]))
    {
        heap_sort(entries, |a, b| order(a).cmp(&order(b)));
    }
}

/// Attach the span from `start` to the reader's position to the decoded `value`, unless the
/// document is broken or mismatched. `layout` names its kind: a record, an array, a variant
/// (2) or a tuple record. `table` is the record's key table (see `std_json_read_field`).
///
/// # Safety
/// `value` is null or an aggregate of that kind; `table` is a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_attach(
    r: *mut u8,
    value: *mut u8,
    start: u32,
    table: *mut u8,
    layout: u32,
) {
    let document = reader(r);
    if value.is_null() || document.broken != 0 || document.mismatch != 0 {
        return;
    }
    const SIZE: usize = mem::size_of::<Source>();
    static SHAPE: [u32; 6] = [
        SIZE as u32,
        4,
        mem::offset_of!(Source, document) as u32,
        mem::offset_of!(Source, table) as u32,
        mem::offset_of!(Source, assigned) as u32,
        mem::offset_of!(Source, cached) as u32,
    ];
    // SAFETY: SHAPE traces each Source slot; only this pool initializes those slots.
    let source = unsafe {
        crate::json_read::pooled_slot(
            &mut document.sources,
            &mut document.source_used,
            SIZE,
            &SHAPE,
        )
        .cast::<Source>()
    };
    unsafe {
        source.write(Source {
            document,
            table,
            start,
            end: document.pos,
            assigned: Buffer::new(),
            cached: ptr::null(),
            cached_len: 0,
            nesting: 0,
            tuple_written: 0,
            tuple: layout == json_source::TUPLE,
        });
        // These are internal headers, independent of the Canonical ABI.
        value
            .add(json_source::offset(layout) as usize)
            .cast::<*mut Source>()
            .write(source);
    }
}

/// Note a write to the aggregate behind `source`: the document stops copying raw text, and
/// an assigned declared `field` (or tuple element `_N`) is serialized even if absent from it.
///
/// # Safety
/// `source` is null or an attached source; `field` is null or a string.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_write(source: *mut u8, field: *mut u8) {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return;
    };
    source.document().dirty = true;
    if field.is_null() {
        return;
    }
    // SAFETY: guaranteed by the caller.
    let key = unsafe { bytes(field) };
    let table = source.table();
    if source.tuple {
        if let Some(index) = key.strip_prefix(b"_").and_then(array_index) {
            source.tuple_written = source.tuple_written.max(index + 1);
        }
        return;
    }
    if table.is_empty() {
        return;
    }
    let index = lookup(table, 0, key);
    if index < 0 {
        return;
    }
    let index = index as u32;
    if source
        .assigned
        .as_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|b| u32::from_le_bytes(*b) == index)
    {
        return;
    }
    source.assigned.extend_from_slice(&index.to_le_bytes());
}

/// Begin serializing a source-backed record: its source members merged with the declared
/// fields assigned since, in property order (see [std_json_source_next]). Null when the
/// record has no source or no declared keys; the serializer then walks its fields alone.
///
/// # Safety
/// `source` is null or an attached source.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_begin(source: *mut u8) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return ptr::null_mut();
    };
    let table = source.table();
    if table.is_empty() {
        return ptr::null_mut();
    }
    let mut members = index_object(source.document, source.start, Some(table));
    let mut order = entries(&mut members)
        .iter()
        .map(|entry| entry.order + 1)
        .max()
        .unwrap_or(0);
    for index in source.assigned.as_slice().as_chunks::<4>().0 {
        let index = u32::from_le_bytes(*index) as usize;
        if entries(&mut members)
            .iter()
            .any(|entry| entry.field == index)
        {
            continue;
        }
        let text = table_key(table, index as u32);
        append(&mut members, Entry::new(text, 0, index, order));
        order += 1;
    }
    sort(&mut members);
    let out = crate::runtime::object!(Members; document, entries);
    unsafe { out.write(members) };
    out.cast()
}

fn raw(writer: *mut u8, text: &[u8]) {
    let writer = json_writer(writer);
    if !writer.failed {
        writer.text.extend_from_slice(text);
    }
}

/// Start an object member named by `entry`'s key.
fn key(writer: *mut u8, entry: &Entry) {
    std_json_item(writer);
    let output = json_writer(writer);
    if !output.failed {
        quote(&mut output.text, key_bytes(entry));
        output.text.push(b':');
    }
}

/// Pending work of [compact]: a value to write, or a container to continue.
enum Frame {
    Value(u32),
    Object(Members),
    /// The offset after the previous element (or the bracket), and whether none came yet.
    Array(u32, bool),
}

/// Write the source value at `start` in canonical `JSON.stringify` form: no whitespace,
/// decoded escapes, normalized numbers and objects in property order. Iterative, bounded by
/// the writer's depth limit.
fn compact(writer: *mut u8, document: *mut Reader, start: u32) {
    let mut stack = ArrayVec::<Frame, { MAX_DEPTH as usize + 1 }>::new();
    stack.push(Frame::Value(start));
    while let Some(frame) = stack.pop() {
        match frame {
            Frame::Value(start) => {
                let mut r = cursor(document, start);
                match r.whitespace() {
                    b'{' => {
                        if std_json_open(writer, b'{' as u32) == 0 {
                            return;
                        }
                        let mut members = index_object(document, r.pos, None);
                        sort(&mut members);
                        stack.push(Frame::Object(members));
                    }
                    b'[' => {
                        if std_json_open(writer, b'[' as u32) == 0 {
                            return;
                        }
                        stack.push(Frame::Array(r.pos + 1, true));
                    }
                    b'"' => {
                        r.pos += 1;
                        let start = r.pos;
                        let (escaped, _) = r.string();
                        if escaped {
                            let output = json_writer(writer);
                            if !output.failed {
                                output.text.push(b'"');
                                decode_string(
                                    &r.input[start as usize..r.pos as usize - 1],
                                    |bytes| quote_contents(&mut output.text, bytes),
                                );
                                output.text.push(b'"');
                            }
                        } else {
                            // The parser already validated this immutable, unescaped string.
                            raw(writer, &r.input[start as usize - 1..r.pos as usize]);
                        }
                    }
                    b'-' | b'0'..=b'9' => {
                        let start = r.pos;
                        r.skip();
                        let literal = &r.input[start as usize..r.pos as usize];
                        let digits = literal.strip_prefix(b"-").unwrap_or(literal);
                        if digits.len() <= 15
                            && digits.iter().all(u8::is_ascii_digit)
                            && literal != b"-0"
                        {
                            // These integers are exact in f64 and already use decimal notation.
                            raw(writer, literal);
                        } else {
                            r.pos = start;
                            let value = r.number();
                            std_json_number(writer, value);
                        }
                    }
                    b't' => raw(writer, b"true"),
                    b'f' => raw(writer, b"false"),
                    _ => raw(writer, b"null"),
                }
            }
            Frame::Object(mut members) => {
                if members.at == entries(&mut members).len() {
                    std_json_close(writer, b'}' as u32);
                    continue;
                }
                let at = members.at;
                let entry = entries(&mut members)[at];
                members.at += 1;
                key(writer, &entry);
                stack.push(Frame::Object(members));
                stack.push(Frame::Value(entry.start as u32));
            }
            Frame::Array(pos, first) => {
                let mut r = cursor(document, pos);
                if r.whitespace() == b']' {
                    std_json_close(writer, b']' as u32);
                    continue;
                }
                if !first {
                    r.pos += 1;
                    r.whitespace();
                }
                let start = r.pos;
                r.skip();
                std_json_item(writer);
                stack.push(Frame::Array(r.pos, false));
                stack.push(Frame::Value(start));
            }
        }
    }
}

/// The next declared field to serialize, writing the undeclared source members before it
/// directly; -1 at the end. Without `members` it counts `index` up to `count`.
///
/// # Safety
/// `members` is null or from [std_json_source_begin]; `writer` is a JSON writer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_next(
    members: *mut u8,
    writer: *mut u8,
    index: u32,
    count: u32,
) -> i32 {
    if members.is_null() {
        return if index < count { index as i32 } else { -1 };
    }
    // SAFETY: guaranteed by the caller.
    let members = unsafe { &mut *members.cast::<Members>() };
    while members.at < entries(members).len() {
        let at = members.at;
        let entry = entries(members)[at];
        members.at += 1;
        if entry.field != usize::MAX {
            return entry.field as i32;
        }
        key(writer, &entry);
        compact(writer, members.document, entry.start as u32);
    }
    -1
}

/// Write the remembered serialization of an unchanged source: 1, or 0 when the caller must
/// serialize it field by field.
///
/// Raw copying is enabled only after observing an identical complete serialization.
/// The observed nesting preserves the depth bound when a cached root becomes a subtree.
///
/// # Safety
/// `source` is null or an attached source; `writer` is a JSON writer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_copy(source: *mut u8, writer: *mut u8) -> u32 {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return 0;
    };
    let document = source.document();
    let writer = json_writer(writer);
    if document.dirty
        || !document.copy_safe
        || writer.failed
        || writer.depth + source.nesting > MAX_DEPTH
    {
        return 0;
    }
    if let Some(cached) = source.cached() {
        writer.max_depth = writer.max_depth.max(writer.depth + source.nesting);
        writer.text.extend_from_slice(cached);
        return 1;
    }
    // Bound speculative capacity: source spans can contain arbitrary whitespace.
    writer
        .text
        .reserve((source.end - source.start).min(4096) as usize);
    0
}

/// Remember the complete serialization `writer` holds for [std_json_source_copy]: the source
/// text itself when identical, else the output, within a budget of the input's length.
///
/// # Safety
/// `source` is null or an attached source; `writer` is a JSON writer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_remember(source: *mut u8, writer: *mut u8) {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return;
    };
    let document = source.document();
    let writer = json_writer(writer);
    if document.dirty || !document.copy_safe || writer.depth != 0 || writer.failed {
        return;
    }
    let text = writer.text.as_slice();
    let original = &document.input[source.start as usize..source.end as usize];
    let cached = if text == original {
        original
    } else if text.len() <= (document.len - document.cached_bytes) as usize {
        document.cached_bytes += text.len() as u32;
        text
    } else {
        return;
    };
    // Source traces either immutable backing allocation directly; no string header is needed.
    source.cached = cached.as_ptr();
    source.cached_len = cached.len() as u32;
    source.nesting = writer.max_depth;
}

/// The serialized length of a tuple: its source elements or the highest one assigned, or
/// `declared` without a source.
///
/// # Safety
/// `source` is null or an attached source.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_tuple_length(
    source: *mut u8,
    declared: u32,
) -> u32 {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return declared;
    };
    let mut scan = cursor(source.document, source.start);
    scan.open(b'[');
    let mut length = 0;
    while scan.element() != 0 {
        scan.skip();
        length += 1;
    }
    length.max(source.tuple_written)
}

/// Write the source elements of a tuple past its `declared` ones.
///
/// # Safety
/// `source` is null or an attached source; `writer` is a JSON writer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_tuple_tail(
    source: *mut u8,
    writer: *mut u8,
    declared: u32,
) {
    // SAFETY: guaranteed by the caller.
    let Some(source) = (unsafe { attached(source) }) else {
        return;
    };
    let mut scan = cursor(source.document, source.start);
    scan.open(b'[');
    let mut index = 0;
    while scan.element() != 0 {
        scan.whitespace();
        let start = scan.pos;
        scan.skip();
        if index >= declared {
            std_json_item(writer);
            compact(writer, source.document, start);
        }
        index += 1;
    }
}
