//! Source spans retained by typed JSON aggregates and consumed by their serializers.
use crate::json::*;
use crate::json_read::{Reader, lookup, pull, reader, std_json_read_string};
use crate::runtime::{Buffer, bytes, number_format};
use core::{mem, ptr, slice};

#[repr(C)]
struct Source {
    document: *mut Reader,
    table: *mut u8,
    start: u32,
    end: u32,
    additions: Buffer,
    canonical: bool,
    tuple_written: u32,
    tuple: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(super) struct Entry {
    pub(super) key: *mut u8,
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
    }
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
    let key = std_json_read_string((r as *mut Reader).cast());
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
        start: start as usize,
        field: usize::MAX,
        order: 0,
    })
}

fn integer_key(key: *mut u8) -> Option<u32> {
    let key = unsafe { bytes(key) };
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
    let mut scan = cursor(document, start);
    scan.open(b'{');
    while let Some(mut entry) = next_member(&mut scan) {
        let key = unsafe { bytes(entry.key) };
        entry.order = members.count;
        if !table.is_null() {
            entry.field = lookup(unsafe { bytes(table) }, 0, key) as usize;
        }
        append(members, entry);
    }
    if members.count > 0 {
        let items = unsafe {
            slice::from_raw_parts_mut(
                members.entries.as_slice().as_ptr() as *mut Entry,
                members.count,
            )
        };
        heap_sort(items, |a, b| {
            unsafe { bytes(a.key).cmp(bytes(b.key)) }.then(a.order.cmp(&b.order))
        });
        let mut count = 0;
        for read in 0..items.len() {
            let mut entry = items[read];
            if count > 0 && unsafe { bytes(items[count - 1].key) == bytes(entry.key) } {
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
    let order = |entry: &Entry| match integer_key(entry.key) {
        Some(index) => (0, index as usize),
        None => (1, entry.order),
    };
    heap_sort(entries, |a, b| order(a).cmp(&order(b)));
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
    static SHAPE: [u32; 5] = [
        SIZE as u32,
        3,
        mem::offset_of!(Source, document) as u32,
        mem::offset_of!(Source, table) as u32,
        mem::offset_of!(Source, additions) as u32,
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
        .additions
        .as_slice()
        .as_chunks::<4>()
        .0
        .iter()
        .any(|b| u32::from_le_bytes(*b) == index)
    {
        return;
    }
    let mut scan = cursor(source.document, source.start);
    scan.open(b'{');
    while let Some(entry) = next_member(&mut scan) {
        if unsafe { bytes(entry.key) == bytes(field) } {
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
        let key = unsafe { crate::runtime::adopt(text.as_ptr() as *mut u8, text.len() as u32) };
        append(
            members,
            Entry {
                key,
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

fn key(writer: *mut u8, key: *mut u8) {
    std_json_item(writer);
    unsafe { std_json_quote(writer, key) };
    raw(writer, b":");
}

#[derive(Clone, Copy)]
enum Frame {
    Value(u32),
    Object(*mut Members),
    Array(u32, bool),
}

fn compact(writer: *mut u8, document: *mut Reader, start: u32) {
    let mut stack = [Frame::Value(0); 129];
    stack[0] = Frame::Value(start);
    let mut depth = 1;
    while depth > 0 {
        depth -= 1;
        match stack[depth] {
            Frame::Value(start) => {
                let mut r = cursor(document, start);
                match r.whitespace() {
                    b'{' => {
                        if std_json_open(writer, b'{' as u32) == 0 {
                            return;
                        }
                        let members = index_object(document, r.pos, ptr::null_mut());
                        sort(unsafe { &mut *members });
                        stack[depth] = Frame::Object(members);
                        depth += 1;
                    }
                    b'[' => {
                        if std_json_open(writer, b'[' as u32) == 0 {
                            return;
                        }
                        stack[depth] = Frame::Array(r.pos + 1, true);
                        depth += 1;
                    }
                    b'"' => {
                        let value = std_json_read_string((&mut r as *mut Reader).cast());
                        unsafe { std_json_quote(writer, value) };
                    }
                    b'-' | b'0'..=b'9' => {
                        let value = r.number();
                        if value.is_finite() {
                            let mut buffer = [0; 40];
                            let len = number_format(value, &mut buffer);
                            raw(writer, &buffer[..len]);
                        } else {
                            raw(writer, b"null");
                        }
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
                key(writer, entry.key);
                stack[depth] = Frame::Object(pointer);
                stack[depth + 1] = Frame::Value(entry.start as u32);
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
                stack[depth] = Frame::Array(r.pos, false);
                stack[depth + 1] = Frame::Value(start);
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
        key(writer, entry.key);
        compact(writer, members.document, entry.start as u32);
    }
    -1
}

/// Raw copying is enabled only after observing an identical complete serialization.
/// Restricting it to the writer root preserves the existing depth bound without a second scan.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_copy(source: *mut u8, writer: *mut u8) -> u32 {
    if source.is_null() {
        return 0;
    }
    let source = unsafe { &*source.cast::<Source>() };
    let document = unsafe { &*source.document };
    let writer = crate::json::writer(writer);
    if !source.canonical
        || document.dirty
        || !document.copy_safe
        || writer.depth != 0
        || writer.failed
    {
        return 0;
    }
    writer
        .text
        .extend_from_slice(&document.input[source.start as usize..source.end as usize]);
    1
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_json_source_remember(source: *mut u8, writer: *mut u8) {
    if source.is_null() {
        return;
    }
    let source = unsafe { &mut *source.cast::<Source>() };
    let document = unsafe { &*source.document };
    let writer = crate::json::writer(writer);
    if document.dirty || !document.copy_safe || writer.depth != 0 || writer.failed {
        return;
    }
    source.canonical =
        writer.text.as_slice() == &document.input[source.start as usize..source.end as usize];
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
