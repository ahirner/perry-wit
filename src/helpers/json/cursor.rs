//! Single-pass lexical cursor for unpublished schema-specific MIR construction.
//! A syntax failure supplies closing delimiters while generated decoders unwind.
//! Dummy values remain inside the atomic construction group, and `finish` raises before publication.
use super::{
    Error, MAX_DEPTH,
    pool::{self, Residue, Text},
    reader::Reader,
};
use arrayvec::ArrayVec;
use core::mem::offset_of;
use perry_abi as runtime;
use perry_abi::json::{
    KEY, METADATA, PACKED_FIELDS, append_field, field_index, field_name, kind, link_child, set_word,
};

struct Container {
    delimiter: u8,
    order: u32,
    graph: *mut u8,
}

#[repr(C)]
struct Cursor {
    input: *mut u8,
    text: &'static str,
    position: usize,
    key: Option<Text<'static>>,
    residue: Residue,
    containers: ArrayVec<Container, { MAX_DEPTH + 1 }>,
    error: Option<Error>,
    scalar_failed: bool,
}
impl Cursor {
    fn text(&self) -> &'static str {
        self.text
    }
    fn reader(&self) -> Reader<'static> {
        let mut reader = Reader::new(self.text());
        reader.position = self.position;
        reader
    }
    fn fail(&mut self, error: Error) {
        self.error.get_or_insert(error);
    }
    fn scalar_depth(&mut self) {
        if self.containers.len() >= MAX_DEPTH {
            self.fail(Error::Depth(self.position));
        }
    }
    fn read_token<T>(
        &mut self,
        fallback: T,
        parse: impl FnOnce(&mut Reader<'static>) -> Result<T, Error>,
    ) -> T {
        if self.error.is_some() {
            return fallback;
        }
        let mut reader = self.reader();
        let result = parse(&mut reader);
        self.position = reader.position;
        match result {
            Ok(value) => value,
            Err(error) => {
                self.fail(error);
                fallback
            }
        }
    }
    fn scalar<T: Default>(
        &mut self,
        kind: super::Kind,
        parse: impl FnOnce(&mut Reader<'static>) -> Result<Option<T>, Error>,
    ) -> T {
        let accepted = self.read_token(false, |r| {
            r.whitespace();
            Ok(matches!(
                (kind, r.peek()),
                (super::Kind::Number, Some(b'-' | b'0'..=b'9'))
                    | (super::Kind::String, Some(b'"'))
                    | (super::Kind::Boolean, Some(b't' | b'f'))
            ))
        });
        self.scalar_depth();
        if !accepted {
            self.discard();
            self.scalar_failed = true;
            return T::default();
        }
        let value = self.read_token(Some(T::default()), parse);
        self.scalar_failed = value.is_none();
        value.unwrap_or_default()
    }
    fn discard(&mut self) {
        if self.error.is_some() {
            return;
        }
        match super::parse::skip(self.text(), self.position, self.containers.len()) {
            Ok(end) => self.position = end,
            Err(error) => self.fail(error),
        }
    }
    fn value(&mut self) -> *mut u8 {
        if self.error.is_some() {
            return core::ptr::null_mut();
        }
        match super::parse::pooled(
            self.text(),
            self.position,
            false,
            self.containers.len(),
            &mut self.residue,
        ) {
            Ok((value, end)) => {
                self.position = end;
                value as usize as *mut u8
            }
            Err(error) => {
                self.fail(error);
                core::ptr::null_mut()
            }
        }
    }
}
unsafe fn cursor<'a>(value: u32) -> &'a mut Cursor {
    unsafe { &mut *(value as usize as *mut Cursor) }
}

/// # Safety
/// `input` is a live Perry string with immutable UTF-8 bytes.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_begin(input: *mut u8) -> u32 {
    // SAFETY: a Perry string owns immutable UTF-8 for the task lifetime.
    let text = unsafe { core::str::from_utf8_unchecked(runtime::bytes(input)) };
    static SHAPE: [u32; 4] = [
        0,
        2,
        offset_of!(Cursor, input) as u32,
        (offset_of!(Cursor, residue) + offset_of!(Residue, data)) as u32,
    ];
    let result = unsafe { runtime::alloc_shaped(core::mem::size_of::<Cursor>() as u32, &SHAPE) }
        as *mut Cursor;
    unsafe {
        result.write(Cursor {
            input,
            text,
            position: 0,
            key: None,
            residue: Residue::new(text),
            containers: ArrayVec::new(),
            error: None,
            scalar_failed: false,
        })
    };
    result as usize as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_byte(value: u32) -> u32 {
    let c = unsafe { cursor(value) };
    if c.error.is_some() {
        return u32::from(
            c.containers
                .last()
                .map_or(0, |frame| if frame.delimiter == b'{' { b'}' } else { b']' }),
        );
    }
    c.read_token(0, |r| {
        r.whitespace();
        Ok(u32::from(r.peek().unwrap_or(0)))
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_consume(value: u32, byte: u32) {
    let c = unsafe { cursor(value) };
    let byte = byte as u8;
    if matches!(byte, b'{' | b'[') {
        c.scalar_depth();
    }
    c.read_token((), |r| r.expect(byte));
    match byte {
        b'{' | b'[' => {
            if c.containers
                .try_push(Container {
                    delimiter: byte,
                    order: 0,
                    graph: core::ptr::null_mut(),
                })
                .is_err()
            {
                c.fail(Error::Depth(c.position));
            }
        }
        b'}' | b']'
            if c.containers.pop().map(|frame| frame.delimiter)
                != Some(if byte == b'}' { b'{' } else { b'[' }) =>
        {
            c.fail(Error::Syntax(c.position));
        }
        _ => {}
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_number(value: u32) -> f64 {
    unsafe { cursor(value) }.scalar(super::Kind::Number, |r| r.numeric().map(Some))
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_i64(value: u32) -> i64 {
    unsafe { cursor(value) }.scalar(super::Kind::Number, Reader::signed_integer)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_u64(value: u32) -> u64 {
    unsafe { cursor(value) }.scalar(super::Kind::Number, Reader::unsigned_integer)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_failed(value: u32) -> u32 {
    u32::from(core::mem::take(&mut unsafe { cursor(value) }.scalar_failed))
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_boolean(value: u32) -> u32 {
    unsafe { cursor(value) }.scalar(super::Kind::Boolean, |r| {
        let yes = r.peek() == Some(b't');
        r.keyword(if yes { b"true" } else { b"false" })?;
        Ok(Some(u32::from(yes)))
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_string(value: u32) -> u32 {
    unsafe { cursor(value) }.scalar(super::Kind::String, |r| {
        Ok(Some(pool::text::<true>(r)?.decoded() as usize as u32))
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_key(
    value: u32,
    table: *const u32,
    count: u32,
    ordered: u32,
) -> u32 {
    let c = unsafe { cursor(value) };
    c.key = c.read_token(None, |r| {
        r.whitespace();
        let key = pool::text::<false>(r)?;
        r.whitespace();
        r.expect(b':')?;
        Ok(Some(key))
    });
    let Some(key) = c.key else {
        return count;
    };
    let index = unsafe { field_index(table, count, |bytes| key.compare(bytes)) };
    if ordered != 0 {
        let frame = c.containers.last_mut().expect("record key inside object");
        if frame.graph.is_null() && count as usize <= PACKED_FIELDS && index < count {
            frame.order = append_field(frame.order, index);
        } else {
            if frame.graph.is_null() {
                frame.graph = pool::json_node(kind::OBJECT, &mut c.residue);
                while frame.order != 0 {
                    let field = (frame.order & 15) - 1;
                    let name = pool::json_node(kind::STRING, &mut c.residue);
                    let bytes = unsafe { runtime::bytes(field_name(table, count, field)) };
                    let item = pool::json_node(kind::UNDEFINED, &mut c.residue);
                    unsafe {
                        set_word(name, 8, bytes.as_ptr() as usize as u32);
                        set_word(name, 12, bytes.len() as u32);
                        set_word(name, 16, runtime::scalars_length(bytes));
                        set_word(item, KEY, name as usize as u32);
                        link_child(frame.graph, item);
                    }
                    frame.order >>= 4;
                }
            }
            if index < count {
                let name = pool::json_text(key, &mut c.residue);
                let item = pool::json_node(kind::UNDEFINED, &mut c.residue);
                unsafe {
                    set_word(item, KEY, name as usize as u32);
                    link_child(frame.graph, item);
                }
            }
        }
    }
    index
}

/// Consume an undeclared record member into its frame's ordered graph.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_extra(value: u32) {
    let graph = unsafe { cursor(value) }
        .containers
        .last()
        .expect("record frame")
        .graph;
    let graph = unsafe { json_cursor_member(value, graph) };
    unsafe { cursor(value) }
        .containers
        .last_mut()
        .expect("record frame")
        .graph = graph as usize as *mut u8;
}

/// Transfer metadata before the closing brace pops this record's frame.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_record(value: u32, native: *mut u8) {
    let c = unsafe { cursor(value) };
    let frame = c.containers.last_mut().expect("record frame");
    unsafe {
        set_word(native, runtime::layout::record::ORDER as usize, frame.order);
        set_word(
            native,
            runtime::layout::record::JSON as usize,
            frame.graph as usize as u32,
        );
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_value(value: u32) -> u32 {
    unsafe { cursor(value) }.value() as usize as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_discard(value: u32) {
    unsafe { cursor(value) }.discard();
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_member(value: u32, graph: *mut u8) -> u32 {
    let c = unsafe { cursor(value) };
    let key = c.key;
    let item = c.value();
    if c.error.is_some() {
        return graph as usize as u32;
    }
    let key = pool::json_text(key.expect("successful current member key"), &mut c.residue);
    let graph = if graph.is_null() {
        pool::json_node(kind::OBJECT, &mut c.residue)
    } else {
        graph
    };
    // The cursor owns this unpublished graph and parsed member.
    unsafe {
        set_word(item, KEY, key as usize as u32);
        link_child(graph, item);
    }
    graph as usize as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_finish(value: u32, failed: u32) {
    let c = unsafe { cursor(value) };
    if c.error.is_none() {
        let mut r = c.reader();
        r.whitespace();
        if r.position != r.input.len() || !c.containers.is_empty() {
            c.fail(Error::Syntax(r.position));
        }
    }
    if let Some(error) = c.error {
        pool::raise_error(c.text(), error);
    } else if failed != 0 {
        runtime::raise(
            runtime::TYPE_ERROR,
            "JSON value does not match the declared schema",
        );
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_tail(value: u32, graph: *mut u8, start: u32) -> u32 {
    let c = unsafe { cursor(value) };
    let item = c.value();
    if c.error.is_some() {
        return graph as usize as u32;
    }
    let graph = if graph.is_null() {
        let g = pool::json_node(kind::ARRAY, &mut c.residue);
        unsafe { set_word(g, METADATA, start) };
        g
    } else {
        graph
    };
    // The cursor owns this unpublished graph and parsed element.
    unsafe { link_child(graph, item) };
    graph as usize as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_seek(value: u32, position: u32) -> u32 {
    let c = unsafe { cursor(value) };
    let old = c.position as u32;
    if position != u32::MAX {
        c.position = position as usize;
    }
    old
}
