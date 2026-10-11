//! Source spans retained by typed JSON aggregates and consumed by their serializers.
use crate::json::*;
use crate::json_read::{Reader, lookup, pull, reader, std_json_read_string};
use crate::runtime::{Buffer, bytes};
use core::{mem, ptr, slice};

#[repr(C)]
struct Source {
    document: *mut Reader,
    table: *mut u8,
    start: u32,
    end: u32,
    additions: Buffer,
    assigned: Buffer,
    cached: *mut u8,
    nesting: u32,
    canonical: bool,
    tuple_written: u32,
    tuple: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Entry {
    key: *const u8,
    key_len: usize,
    pub(super) start: usize,
    field: usize,
    order: usize,
}

#[repr(C)]
struct Members {
    document: *mut Reader,
    entries: Buffer,
    at: usize,
    count: usize,
}

fn cursor(document: *mut Reader, start: u32) -> Reader {
    // SAFETY: provenance retains the document and its immutable input.
    let document = unsafe { &*document };
    let mut cursor =
        pull::Cursor::new(unsafe { core::str::from_utf8_unchecked(bytes(document.text)) });
    cursor.pos = start;
    Reader {
        text: document.text,
        data: document.data,
        cursor,
        mismatch: 0,
        dirty: false,
        copy_safe: false,
        payload: u32::MAX,
        sources: ptr::null_mut(),
        source_used: 64,
        strings: ptr::null_mut(),
        string_used: 64,
        pooled: false,
        cached_bytes: 0,
    }
}

pub(super) fn key_bytes(entry: &Entry) -> &[u8] {
    // SAFETY: Members traces the immutable source or decoded key allocation.
    unsafe { slice::from_raw_parts(entry.key, entry.key_len) }
}

fn entries(members: &Members) -> &[Entry] {
    if members.count == 0 {
        return &[];
    }
    // SAFETY: only complete, initialized entries are appended to this aligned buffer.
    unsafe { slice::from_raw_parts(members.entries.as_slice().as_ptr().cast(), members.count) }
}

fn append(members: &mut Members, entry: Entry) {
    // SAFETY: Entry consists solely of pointer-sized initialized words, without padding.
    let bytes =
        unsafe { slice::from_raw_parts((&entry as *const Entry).cast(), mem::size_of::<Entry>()) };
    let offset = members.count * mem::size_of::<Entry>();
    if offset < members.entries.len() {
        unsafe {
            ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                members.entries.as_slice().as_ptr().add(offset) as *mut u8,
                bytes.len(),
            )
        };
    } else {
        members.entries.extend_from_slice(bytes);
    }
    members.count += 1;
}

pub(super) fn next_member(r: &mut Reader) -> Option<Entry> {
    if r.broken != 0 {
        return None;
    }
    let next = r.whitespace();
    if next == b'}' {
        r.close();
        return None;
    }
    if mem::take(&mut r.fresh) == 0 {
        if next != b',' {
            r.broken = 1;
            return None;
        }
        r.pos += 1;
    }
    if r.whitespace() != b'"' {
        r.broken = 1;
        return None;
    }
    r.pos += 1;
    let start = r.pos;
    let (escaped, _) = r.string();
    if r.broken != 0 {
        return None;
    }
    let key = if escaped {
        r.pos = start - 1;
        unsafe { bytes(std_json_read_string((r as *mut Reader).cast())) }
    } else {
        &r.input[start as usize..r.pos.saturating_sub(1) as usize]
    };
    let (key, key_len) = (key.as_ptr(), key.len());
    if r.whitespace() != b':' || r.broken != 0 {
        r.broken = 1;
        return None;
    }
    r.pos += 1;
    r.whitespace();
    let start = r.pos;
    r.skip();
    Some(Entry {
        key,
        key_len,
        start: start as usize,
        field: usize::MAX,
        order: 0,
    })
}

fn integer_key(key: &[u8]) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key[0] == b'0') {
        return None;
    }
    let mut value = 0u32;
    for &digit in key {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(digit - b'0'))?;
    }
    (value != u32::MAX).then_some(value)
}

fn index_object(document: *mut Reader, start: u32, table: *mut u8) -> *mut Members {
    static SHAPE: [u32; 3] = [mem::size_of::<Entry>() as u32, 1, 0];
    let out = crate::runtime::object!(Members; document, entries);
    unsafe {
        out.write(Members {
            document,
            entries: Buffer::with_shape(&SHAPE),
            at: 0,
            count: 0,
        })
    };
    let members = unsafe { &mut *out };
    let mut pending = [Entry {
        key: ptr::null(),
        key_len: 0,
        start: 0,
        field: usize::MAX,
        order: 0,
    }; 128];
    let mut count = 0;
    let mut scan = cursor(document, start);
    scan.open(b'{');
    while let Some(mut entry) = next_member(&mut scan) {
        entry.order = count;
        let key = key_bytes(&entry);
        if !table.is_null() {
            entry.field = lookup(unsafe { bytes(table) }, 0, key) as usize;
        }
        if count < pending.len() {
            pending[count] = entry;
        } else {
            if count == pending.len() {
                // Entries have no padding and their keys stay live through the document.
                members.entries.extend_from_slice(unsafe {
                    slice::from_raw_parts(pending.as_ptr().cast(), mem::size_of_val(&pending))
                });
                members.count = count;
            }
            append(members, entry);
        }
        count += 1;
    }
    if count <= pending.len() {
        members.count = index_small_object(&mut pending[..count]);
        if members.count != 0 {
            members.entries.extend_from_slice(unsafe {
                slice::from_raw_parts(
                    pending.as_ptr().cast(),
                    members.count * mem::size_of::<Entry>(),
                )
            });
        }
        return out;
    }
    if members.count > 0 {
        let items = unsafe {
            slice::from_raw_parts_mut(
                members.entries.as_slice().as_ptr() as *mut Entry,
                members.count,
            )
        };
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
    out
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
    let entries = unsafe {
        slice::from_raw_parts_mut(members.entries.as_slice().as_ptr() as *mut Entry, count)
    };
    let order = |entry: &Entry| match integer_key(key_bytes(entry)) {
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

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_read_attach(
    r: *mut u8,
    value: *mut u8,
    start: u32,
    table: *mut u8,
    array: u32,
) {
    let document = reader(r);
    if value.is_null() || document.broken != 0 || document.mismatch != 0 {
        return;
    }
    const SIZE: usize = mem::size_of::<Source>();
    static SHAPE: [u32; 7] = [
        SIZE as u32,
        5,
        mem::offset_of!(Source, document) as u32,
        mem::offset_of!(Source, table) as u32,
        mem::offset_of!(Source, additions) as u32,
        mem::offset_of!(Source, assigned) as u32,
        mem::offset_of!(Source, cached) as u32,
    ];
    if document.source_used == 64 {
        document.sources = unsafe { crate::runtime::alloc_shaped((SIZE * 64) as u32, &SHAPE) };
        unsafe { ptr::write_bytes(document.sources, 0, SIZE * 64) };
        document.source_used = 0;
    }
    let source = unsafe {
        document
            .sources
            .add(document.source_used as usize * SIZE)
            .cast::<Source>()
    };
    document.source_used += 1;
    unsafe {
        source.write(Source {
            document,
            table,
            start,
            end: document.pos,
            additions: Buffer::new(),
            assigned: Buffer::new(),
            cached: ptr::null_mut(),
            nesting: 0,
            canonical: false,
            tuple_written: 0,
            tuple: array == 3,
        });
        // These are internal headers, independent of the Canonical ABI.
        value
            .add(match array {
                0 | 3 => crate::runtime::layout::record::SOURCE,
                1 => crate::runtime::layout::array::SOURCE,
                _ => crate::runtime::layout::variant::SOURCE,
            } as usize)
            .cast::<*mut Source>()
            .write(source);
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_write(source: *mut u8, field: *mut u8) {
    if source.is_null() {
        return;
    }
    let source = unsafe { &mut *source.cast::<Source>() };
    unsafe { (*source.document).dirty = true };
    if field.is_null() {
        return;
    }
    if source.tuple {
        let key = unsafe { bytes(field) };
        if key.first() == Some(&b'_') {
            let mut index = 0u32;
            for &digit in &key[1..] {
                index = index * 10 + u32::from(digit - b'0');
            }
            source.tuple_written = source.tuple_written.max(index + 1);
        }
        return;
    }
    if unsafe { bytes(source.table) }.is_empty() {
        return;
    }
    let index = lookup(unsafe { bytes(source.table) }, 0, unsafe { bytes(field) });
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
    let mut scan = cursor(source.document, source.start);
    scan.open(b'{');
    while let Some(entry) = next_member(&mut scan) {
        if key_bytes(&entry) == unsafe { bytes(field) } {
            return;
        }
    }
    source.additions.extend_from_slice(&index.to_le_bytes());
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_begin(source: *mut u8) -> *mut u8 {
    if source.is_null() {
        return ptr::null_mut();
    }
    let source = unsafe { &*source.cast::<Source>() };
    if unsafe { bytes(source.table) }.is_empty() {
        return ptr::null_mut();
    }
    let members = index_object(source.document, source.start, source.table);
    let members = unsafe { &mut *members };
    let table = unsafe { bytes(source.table) };
    for index in source.additions.as_slice().as_chunks::<4>().0 {
        let index = u32::from_le_bytes(*index) as usize;
        let text = crate::json_read::table_key(table, index as u32);

        append(
            members,
            Entry {
                key: text.as_ptr(),
                key_len: text.len(),
                start: 0,
                field: index,
                order: entries(members)
                    .iter()
                    .map(|entry| entry.order + 1)
                    .max()
                    .unwrap_or(0),
            },
        );
    }
    sort(members);
    (members as *mut Members).cast()
}

fn raw(writer: *mut u8, text: &[u8]) {
    unsafe { std_json_raw(writer, text.as_ptr(), text.len() as u32) };
}

fn key(writer: *mut u8, entry: &Entry) {
    std_json_item(writer);
    let output = crate::json::writer(writer);
    if !output.failed {
        crate::json::quote(&mut output.text, key_bytes(entry));
    }
    raw(writer, b":");
}

#[derive(Clone, Copy)]
enum Frame {
    Value(u32),
    Object(*mut Members),
    Array(u32, bool),
}

fn compact(writer: *mut u8, document: *mut Reader, start: u32) {
    let mut stack = [const { mem::MaybeUninit::<Frame>::uninit() }; 129];
    stack[0].write(Frame::Value(start));
    let mut depth = 1;
    while depth > 0 {
        depth -= 1;
        // Every live stack entry is written before increasing depth; popped entries are Copy.
        match unsafe { stack[depth].assume_init_read() } {
            Frame::Value(start) => {
                let mut r = cursor(document, start);
                match r.whitespace() {
                    b'{' => {
                        if std_json_open(writer, b'{' as u32) == 0 {
                            return;
                        }
                        let members = index_object(document, r.pos, ptr::null_mut());
                        sort(unsafe { &mut *members });
                        stack[depth].write(Frame::Object(members));
                        depth += 1;
                    }
                    b'[' => {
                        if std_json_open(writer, b'[' as u32) == 0 {
                            return;
                        }
                        stack[depth].write(Frame::Array(r.pos + 1, true));
                        depth += 1;
                    }
                    b'"' => {
                        r.pos += 1;
                        let start = r.pos;
                        let (escaped, _) = r.string();
                        if escaped {
                            r.pos = start - 1;
                            let value = std_json_read_string((&mut r as *mut Reader).cast());
                            unsafe { std_json_quote(writer, value) };
                        } else {
                            // The parser already validated this immutable, unescaped string.
                            raw(writer, &r.input[start as usize - 1..r.pos as usize]);
                        }
                    }
                    b'-' | b'0'..=b'9' => {
                        let value = r.number();
                        std_json_number(writer, value);
                    }
                    b't' => raw(writer, b"true"),
                    b'f' => raw(writer, b"false"),
                    _ => raw(writer, b"null"),
                }
            }
            Frame::Object(pointer) => {
                let members = unsafe { &mut *pointer };
                if members.at == entries(members).len() {
                    std_json_close(writer, b'}' as u32);
                    continue;
                }
                let entry = entries(members)[members.at];
                members.at += 1;
                key(writer, &entry);
                stack[depth].write(Frame::Object(pointer));
                stack[depth + 1].write(Frame::Value(entry.start as u32));
                depth += 2;
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
                stack[depth].write(Frame::Array(r.pos, false));
                stack[depth + 1].write(Frame::Value(start));
                depth += 2;
            }
        }
    }
}

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
    let members = unsafe { &mut *members.cast::<Members>() };
    while members.at < entries(members).len() {
        let entry = entries(members)[members.at];
        members.at += 1;
        if entry.field != usize::MAX {
            return entry.field as i32;
        }
        key(writer, &entry);
        compact(writer, members.document, entry.start as u32);
    }
    -1
}

/// Raw copying is enabled only after observing an identical complete serialization.
/// The observed nesting preserves the depth bound when a cached root becomes a subtree.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_copy(source: *mut u8, writer: *mut u8) -> u32 {
    if source.is_null() {
        return 0;
    }
    let source = unsafe { &*source.cast::<Source>() };
    let document = unsafe { &mut *source.document };
    let writer = crate::json::writer(writer);
    if document.dirty
        || !document.copy_safe
        || writer.failed
        || writer.depth + source.nesting > MAX_DEPTH
    {
        return 0;
    }
    if !source.cached.is_null() {
        writer.max_depth = writer.max_depth.max(writer.depth + source.nesting);
        writer
            .text
            .extend_from_slice(unsafe { bytes(source.cached) });
        return 1;
    }
    if source.canonical {
        writer.max_depth = writer.max_depth.max(writer.depth + source.nesting);
        writer
            .text
            .extend_from_slice(&document.input[source.start as usize..source.end as usize]);
        return 1;
    }
    0
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_remember(source: *mut u8, writer: *mut u8) {
    if source.is_null() {
        return;
    }
    let source = unsafe { &mut *source.cast::<Source>() };
    let document = unsafe { &mut *source.document };
    let writer = crate::json::writer(writer);
    if document.dirty || !document.copy_safe || writer.depth != 0 || writer.failed {
        return;
    }
    let text = writer.text.as_slice();
    source.nesting = writer.max_depth;
    source.canonical = text == &document.input[source.start as usize..source.end as usize];
    // Retain at most one input document's worth of normalized output. The writer's
    // immutable bytes already belong to the traced heap; only a view is allocated.
    if !source.canonical && text.len() <= (document.len - document.cached_bytes) as usize {
        source.cached =
            unsafe { crate::runtime::adopt(text.as_ptr() as *mut u8, text.len() as u32) };
        document.cached_bytes += text.len() as u32;
    }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_tuple_length(
    source: *mut u8,
    declared: u32,
) -> u32 {
    if source.is_null() {
        return declared;
    }
    let source = unsafe { &*source.cast::<Source>() };
    let mut scan = cursor(source.document, source.start);
    scan.open(b'[');
    let mut length = 0;
    while crate::json_read::std_json_read_element((&mut scan as *mut Reader).cast()) != 0 {
        scan.skip();
        length += 1;
    }
    length.max(source.tuple_written)
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_tuple_tail(
    source: *mut u8,
    writer: *mut u8,
    declared: u32,
) {
    if source.is_null() {
        return;
    }
    let source = unsafe { &*source.cast::<Source>() };
    let mut scan = cursor(source.document, source.start);
    scan.open(b'[');
    let mut index = 0;
    while crate::json_read::std_json_read_element((&mut scan as *mut Reader).cast()) != 0 {
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
