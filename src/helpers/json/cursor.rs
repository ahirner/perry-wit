//! Single-pass lexical cursor for unpublished schema-specific MIR construction.
//! A syntax failure supplies closing delimiters while generated decoders unwind.
//! Dummy values remain inside the atomic construction group; finish raises before publication.
use super::{
    Error, MAX_DEPTH,
    pool::{self, Residue, Text},
    reader::Reader,
};
use arrayvec::ArrayVec;
use core::mem::offset_of;
use perry_abi as runtime;

#[repr(C)]
struct Cursor {
    input: *mut u8,
    text: &'static str,
    position: usize,
    key: Option<Text<'static>>,
    residue: Residue,
    containers: ArrayVec<u8, { MAX_DEPTH + 1 }>,
    error: Option<Error>,
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
fn word(value: *mut u8, offset: usize) -> u32 {
    unsafe { value.add(offset).cast::<u32>().read() }
}
fn set_word(value: *mut u8, offset: usize, data: u32) {
    unsafe { value.add(offset).cast::<u32>().write(data) }
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_begin(input: *mut u8) -> u32 {
    let text = unsafe { runtime::bytes(input) };
    let text = match core::str::from_utf8(text) {
        Ok(text) => text,
        Err(_) => {
            runtime::raise(runtime::SYNTAX_ERROR, "JSON input is not UTF-8");
            return 0;
        }
    };
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
                .map_or(0, |byte| if *byte == b'{' { b'}' } else { b']' }),
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
            if c.containers.try_push(byte).is_err() {
                c.fail(Error::Depth(c.position));
            }
        }
        b'}' | b']' if c.containers.pop() != Some(if byte == b'}' { b'{' } else { b'[' }) => {
            c.fail(Error::Syntax(c.position));
        }
        _ => {}
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_number(value: u32) -> f64 {
    let c = unsafe { cursor(value) };
    c.scalar_depth();
    c.read_token(0.0, Reader::numeric)
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_boolean(value: u32) -> u32 {
    let c = unsafe { cursor(value) };
    c.scalar_depth();
    c.read_token(0, |r| {
        let yes = r.peek() == Some(b't');
        r.keyword(if yes { b"true" } else { b"false" })?;
        Ok(u32::from(yes))
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_string(value: u32) -> u32 {
    let c = unsafe { cursor(value) };
    c.scalar_depth();
    c.read_token(0, |r| {
        Ok(pool::text::<true>(r)?.decoded(r.input) as usize as u32)
    })
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_key(value: u32, table: *const u32, count: u32) -> u32 {
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
    let names = unsafe { core::slice::from_raw_parts(table, count as usize * 2) }
        .as_chunks::<2>()
        .0;
    let compare = |entry: &[u32; 2]| {
        key.compare(c.text(), unsafe {
            runtime::bytes(entry[0] as usize as *mut u8)
        })
        .reverse()
    };
    let index = if count < 16 {
        names.iter().position(|entry| compare(entry).is_eq())
    } else {
        names.binary_search_by(compare).ok()
    };
    index.map_or(count, |i| names[i][1])
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_value(value: u32) -> u32 {
    unsafe { cursor(value) }.value() as usize as u32
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_discard(value: u32) {
    let c = unsafe { cursor(value) };
    if c.error.is_some() {
        return;
    }
    match super::parse::skip(c.text(), c.position, c.containers.len()) {
        Ok(end) => c.position = end,
        Err(error) => c.fail(error),
    }
}
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_cursor_member(value: u32, graph: *mut u8) -> u32 {
    let c = unsafe { cursor(value) };
    let key = c.key;
    let item = c.value();
    if c.error.is_some() {
        return graph as usize as u32;
    }
    let key = pool::json_text(
        key.expect("successful current member key"),
        c.text(),
        &mut c.residue,
    );
    set_word(item, 24, key as usize as u32);
    let graph = if graph.is_null() {
        pool::json_node(6, &mut c.residue)
    } else {
        graph
    };
    match word(graph, 8) {
        0 => set_word(graph, 16, item as usize as u32),
        last => set_word(last as usize as *mut u8, 4, item as usize as u32),
    }
    set_word(graph, 8, item as usize as u32);
    set_word(graph, 20, word(graph, 20) + 1);
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
        let at = match error {
            Error::Syntax(at) | Error::UnpairedSurrogate(at) | Error::Depth(at) => at,
            Error::Capacity => 0,
        };
        let message = runtime::json_syntax_message(c.text().as_bytes(), at as u32);
        runtime::raise_string(runtime::SYNTAX_ERROR, message);
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
        let g = pool::json_node(5, &mut c.residue);
        set_word(g, 28, start);
        g
    } else {
        graph
    };
    match word(graph, 8) {
        0 => set_word(graph, 16, item as usize as u32),
        last => set_word(last as usize as *mut u8, 4, item as usize as u32),
    }
    set_word(graph, 8, item as usize as u32);
    set_word(graph, 20, word(graph, 20) + 1);
    graph as usize as u32
}
