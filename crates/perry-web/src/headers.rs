//! `Headers`: lowercased names and Latin-1 values in insertion order.
use crate::*;

/// A header list: 16-byte fields (`name`, `name_len`, `value`, `value_len`) in insertion
/// order, the layout the fetch helper reads.
#[repr(C)]
pub struct Headers {
    fields: Buffer,
}

fn field_buffer() -> Buffer {
    // SAFETY: every 16-byte entry contains name and value pointers at offsets 0 and 8.
    unsafe { Buffer::with_shape(&[16, 2, 0, 8]) }
}

fn headers<'a>(h: *mut u8) -> &'a mut Headers {
    // SAFETY: compiled code passes header lists.
    unsafe { &mut *(h as *mut Headers) }
}

impl Headers {
    fn fields(&self) -> impl Iterator<Item = (&'static [u8], &'static [u8])> + '_ {
        self.fields
            .as_slice()
            .as_chunks::<16>()
            .0
            .iter()
            .map(|field| {
                let word = |at: usize| u32::from_le_bytes(field[at..at + 4].try_into().unwrap());
                // SAFETY: fields point at bytes in the task region.
                unsafe { (slice(word(0), word(4)), slice(word(8), word(12))) }
            })
    }

    fn push(&mut self, name: &[u8], value: &[u8]) {
        for word in [
            name.as_ptr() as u32,
            name.len() as u32,
            value.as_ptr() as u32,
            value.len() as u32,
        ] {
            self.fields.extend_from_slice(&word.to_le_bytes());
        }
    }

    /// Keep the fields `keep` accepts, in order.
    fn retain(&mut self, mut keep: impl FnMut(&[u8]) -> bool) {
        let mut kept = field_buffer();
        for (field, (name, _)) in self
            .fields
            .as_slice()
            .as_chunks::<16>()
            .0
            .iter()
            .zip(self.fields())
        {
            if keep(name) {
                kept.extend_from_slice(field);
            }
        }
        self.fields = kept;
    }
}

/// A new empty header list.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_new() -> *mut u8 {
    let h = perry_abi::object!(Headers; fields);
    // SAFETY: a fresh block.
    unsafe {
        h.write(Headers {
            fields: field_buffer(),
        })
    };
    h as *mut u8
}

/// A copy of a header list (`new Headers(headers)`, constructor `headers` init).
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_copy(source: *mut u8) -> *mut u8 {
    let copy = web_headers_new();
    headers(copy)
        .fields
        .extend_from_slice(headers(source).fields.as_slice());
    copy
}

/// The lowercased name, or `TypeError` naming `operation`.
fn name(operation: &[u8], name: *mut u8) -> Option<&'static [u8]> {
    // SAFETY: compiled code passes strings.
    let text = unsafe { bytes(name) };
    let output = alloc(text.len() as u32);
    // SAFETY: helper call with an input string and a disjoint output of the same size.
    let len = unsafe {
        helper::fetch_header_name(text.as_ptr(), text.len() as u32, output, text.len() as u32)
    };
    if len == INVALID {
        type_error(&[operation, b": \"", text, b"\" is an invalid header name."]);
        return None;
    }
    // SAFETY: the helper wrote `len` bytes.
    Some(unsafe { slice(output as u32, len) })
}

/// The trimmed Latin-1 value, or `TypeError` naming `operation`.
fn value(operation: &[u8], value: *mut u8) -> Option<&'static [u8]> {
    // SAFETY: compiled code passes strings.
    let text = unsafe { bytes(value) };
    let output = alloc(text.len() as u32);
    // SAFETY: helper call with an input string and a disjoint output of the same size.
    let len = unsafe {
        helper::fetch_header_value(text.as_ptr(), text.len() as u32, output, text.len() as u32)
    };
    if len == INVALID {
        type_error(&[operation, b": \"", text, b"\" is an invalid header value."]);
        return None;
    }
    // SAFETY: the helper wrote `len` bytes.
    Some(unsafe { slice(output as u32, len) })
}

/// `headers.append(name, value)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_append(h: *mut u8, key: *mut u8, text: *mut u8) {
    let Some(key) = name(b"Headers.append", key) else {
        return;
    };
    let Some(text) = value(b"Headers.append", text) else {
        return;
    };
    headers(h).push(key, text);
}

/// `headers.set(name, value)`: replaces the first field of `name` and drops the others.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_set(h: *mut u8, key: *mut u8, text: *mut u8) {
    let Some(key) = name(b"Headers.set", key) else {
        return;
    };
    let Some(text) = value(b"Headers.set", text) else {
        return;
    };
    let list = headers(h);
    let mut replaced = field_buffer();
    let mut found = false;
    for (field, (name, _)) in list
        .fields
        .as_slice()
        .as_chunks::<16>()
        .0
        .iter()
        .zip(list.fields())
    {
        if name != key {
            replaced.extend_from_slice(field);
        } else if !found {
            found = true;
            for word in [
                key.as_ptr() as u32,
                key.len() as u32,
                text.as_ptr() as u32,
                text.len() as u32,
            ] {
                replaced.extend_from_slice(&word.to_le_bytes());
            }
        }
    }
    list.fields = replaced;
    if !found {
        list.push(key, text);
    }
}

/// `headers.get(name)`: the values joined with `, `, as UTF-8; null when absent.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_get(h: *mut u8, key: *mut u8) -> *mut u8 {
    let Some(key) = name(b"Headers.get", key) else {
        return core::ptr::null_mut();
    };
    let mut joined = Buffer::new();
    let mut found = false;
    for (name, value) in headers(h).fields() {
        if name != key {
            continue;
        }
        if found {
            joined.extend_from_slice(b", ");
        }
        found = true;
        for &byte in value {
            if byte < 0x80 {
                joined.push(byte);
            } else {
                joined.extend_from_slice(&[0xC0 | (byte >> 6), 0x80 | (byte & 0x3F)]);
            }
        }
    }
    if !found {
        return core::ptr::null_mut();
    }
    from_bytes(joined.as_slice())
}

/// `headers.has(name)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_has(h: *mut u8, key: *mut u8) -> u32 {
    let Some(key) = name(b"Headers.has", key) else {
        return 0;
    };
    u32::from(headers(h).fields().any(|(name, _)| name == key))
}

/// `headers.delete(name)`.
#[unsafe(no_mangle)]
pub extern "C" fn web_headers_delete(h: *mut u8, key: *mut u8) {
    let Some(key) = name(b"Headers.delete", key) else {
        return;
    };
    headers(h).retain(|name| name != key);
}

pub(crate) fn set_default(h: *mut u8, key: &'static [u8], text: &'static [u8]) {
    let list = headers(h);
    if !list.fields().any(|(name, _)| name == key) {
        list.push(key, text);
    }
}

/// The `(name, value)` fields of header list `h`.
pub(crate) fn fields(h: *mut u8) -> impl Iterator<Item = (&'static [u8], &'static [u8])> {
    headers(h).fields()
}

/// A header list over `count` canonical `(name, value)` fields at `list` (a fetched
/// response's headers, lowered into the task region by the host).
pub(crate) fn adopt_fields(list: *mut u8, count: u32) -> *mut u8 {
    let h = web_headers_new();
    headers(h).fields = adopt_field_buffer(list, count);
    h
}

/// Native pointer-map storage over the canonical header payloads.
pub(crate) fn adopt_field_buffer(list: *mut u8, count: u32) -> Buffer {
    let mut fields = field_buffer();
    // SAFETY: `count` 16-byte fields at `list`.
    fields.extend_from_slice(unsafe { slice(list as u32, count * 16) });
    // SAFETY: the copied field buffer traces the original canonical name/value payloads.
    unsafe {
        for index in 0..count as usize {
            let field = list.add(index * 16).cast::<u32>();
            perry_abi::release_canonical(*field as usize as *mut u8);
            perry_abi::release_canonical(*field.add(2) as usize as *mut u8);
        }
        perry_abi::release_canonical(list);
    }
    fields
}
