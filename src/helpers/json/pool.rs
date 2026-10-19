//! Pooled graph storage for dynamic JSON and undeclared typed members.
use super::{Error, reader::Reader, storage::GraphStorage};
use perry_abi as runtime;
use perry_abi::json::{NODE_BYTES, kind, set_word};

#[derive(Clone, Copy)]
pub(super) struct Text<'a> {
    literal: &'a str,
    bytes: u32,
    scalars: Option<u32>,
    escaped: bool,
}
impl Text<'_> {
    fn raw(&self) -> &str {
        &self.literal[1..self.literal.len() - 1]
    }
    fn data_in(self, allocate: impl FnOnce(u32) -> *mut u8) -> *mut u8 {
        if !self.escaped {
            self.raw().as_ptr().cast_mut()
        } else {
            let data = allocate(self.bytes);
            let mut at = 0;
            let mut reader = Reader::new(self.literal);
            reader
                .string::<false>(|chunk| {
                    unsafe {
                        core::ptr::copy_nonoverlapping(chunk.as_ptr(), data.add(at), chunk.len())
                    };
                    at += chunk.len();
                    Ok(())
                })
                .expect("input was validated");
            data
        }
    }
    pub(super) fn decoded(self) -> *mut u8 {
        unsafe {
            runtime::adopt_counted(
                self.data_in(runtime::alloc),
                self.bytes,
                self.scalar_count(),
            )
        }
    }
    fn scalar_count(self) -> u32 {
        if let Some(count) = self.scalars {
            return count;
        }
        if !self.escaped {
            return runtime::scalars_length(self.raw().as_bytes());
        }
        let mut reader = Reader::new(self.literal);
        reader
            .string::<true>(|_| Ok(()))
            .expect("key was validated")
            .scalars
            .unwrap()
    }
    pub(super) fn compare(self, mut expected: &[u8]) -> core::cmp::Ordering {
        let length = (self.bytes as usize).cmp(&expected.len());
        if !length.is_eq() {
            return length;
        }
        if !self.escaped {
            return self.raw().as_bytes().cmp(expected);
        }
        let mut ordering = core::cmp::Ordering::Equal;
        let mut reader = Reader::new(self.literal);
        reader
            .string::<false>(|chunk| {
                let (head, rest) = expected.split_at(chunk.len());
                expected = rest;
                if ordering.is_eq() {
                    ordering = chunk.cmp(head);
                }
                Ok(())
            })
            .expect("key was validated");
        ordering
    }
}

pub(super) fn text<'a, const COUNT: bool>(reader: &mut Reader<'a>) -> Result<Text<'a>, Error> {
    let start = reader.position;
    let facts = reader.string::<COUNT>(|_| Ok(()))?;
    let end = reader.position;
    Ok(Text {
        literal: &reader.input[start..end],
        bytes: facts.bytes,
        scalars: facts.scalars,
        escaped: facts.escaped,
    })
}

pub(super) struct Residue {
    input: *const u8,
    pub(super) data: *mut u8,
    used: u32,
    capacity: u32,
    hint: u32,
}
impl Residue {
    pub(super) fn new(input: &str) -> Self {
        Self {
            input: input.as_ptr(),
            data: core::ptr::null_mut(),
            used: 0,
            capacity: 0,
            hint: u32::try_from(input.len())
                .expect("Wasm input size")
                .clamp(128, 512),
        }
    }
    pub(super) fn alloc(&mut self, size: u32, align: u32) -> *mut u8 {
        self.used = (self.used + align - 1) & !(align - 1);
        let end = self.used.checked_add(size).expect("residue size");
        if self.data.is_null() || end > self.capacity {
            static OWNER: [u32; 5] = [0, 3, 0, 4, 8];
            let capacity = size
                .checked_add(16)
                .expect("residue size")
                .max(self.hint)
                .max(self.capacity.saturating_mul(2));
            let data = unsafe { runtime::alloc_shaped(capacity, &OWNER) };
            unsafe {
                set_word(data, 0, 0);
                set_word(data, 4, 0);
                set_word(data, 8, self.data as usize as u32);
                if !self.data.is_null() {
                    set_word(self.data, 4, data as usize as u32);
                }
            }
            self.data = data;
            self.capacity = capacity;
            self.used = 16;
        }
        let out = unsafe { self.data.add(self.used as usize) };
        self.used += size;
        out
    }
}
pub(super) fn json_node(kind: u32, residue: &mut Residue) -> *mut u8 {
    let node = residue.alloc(NODE_BYTES, 8);
    unsafe {
        core::ptr::write_bytes(node, 0, NODE_BYTES as usize);
        set_word(node, 0, kind);
    }
    node
}
pub(super) fn json_text(value: Text<'_>, residue: &mut Residue) -> *mut u8 {
    let size = NODE_BYTES
        .checked_add(if value.escaped { value.bytes } else { 0 })
        .expect("string size");
    let node = residue.alloc(size, 8);
    let data = value.data_in(|_| unsafe { node.add(NODE_BYTES as usize) });
    unsafe {
        core::ptr::write_bytes(node, 0, NODE_BYTES as usize);
        set_word(node, 0, kind::STRING);
        set_word(node, 8, data as usize as u32);
        set_word(node, 12, value.bytes);
        set_word(node, 16, value.scalar_count());
        if !value.escaped && value.bytes != 0 {
            set_word(residue.data, 0, residue.input as usize as u32);
        }
    }
    node
}

pub(super) struct Pooled<'a>(pub(super) &'a mut Residue);
impl GraphStorage for Pooled<'_> {
    const STORE: bool = true;
    fn reserve(&mut self, size: usize, alignment: usize) -> Result<usize, Error> {
        let size = u32::try_from(size).map_err(|_| Error::Capacity)?;
        let alignment = u32::try_from(alignment).map_err(|_| Error::Capacity)?;
        let node = self.0.alloc(size, alignment);
        unsafe { core::ptr::write_bytes(node, 0, size as usize) };
        Ok(node as usize)
    }
    fn write(&mut self, position: usize, bytes: &[u8]) -> Result<(), Error> {
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), position as *mut u8, bytes.len()) };
        Ok(())
    }
    fn address(&self, position: usize) -> Result<u32, Error> {
        u32::try_from(position).map_err(|_| Error::Capacity)
    }
    fn string(&mut self, reader: &mut Reader<'_>) -> Result<usize, Error> {
        let value = text::<true>(reader)?;
        Ok(json_text(value, self.0) as usize)
    }
}

pub(super) fn raise_error(input: &str, error: Error) {
    let offset = match error {
        Error::Syntax(i) | Error::UnpairedSurrogate(i) | Error::Depth(i) => i,
        Error::Capacity => 0,
    };
    let message = runtime::json_syntax_message(input.as_bytes(), offset as u32);
    runtime::raise_string(runtime::SYNTAX_ERROR, message);
}

/// # Safety
/// `string` is a live Perry string with immutable UTF-8 bytes.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_parse_string(string: *mut u8) -> u32 {
    let input = unsafe { core::str::from_utf8_unchecked(runtime::bytes(string)) };
    let mut residue = Residue::new(input);
    match super::parse::pooled(input, 0, true, 0, &mut residue) {
        Ok((value, _)) => value,
        Err(error) => {
            raise_error(input, error);
            0
        }
    }
}
