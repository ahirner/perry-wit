use super::{Error, Kind, MAX_DEPTH, NODE_BYTES, storage::Storage};

/// Validates a document and computes its exact node/data storage requirement.
pub fn measure(input: &str) -> Result<usize, Error> {
    let mut parser = Parser {
        input,
        position: 0,
        output: Storage {
            bytes: &mut [],
            length: 0,
            base: 0,
            stores: false,
        },
    };
    parser.document()?;
    Ok(parser.output.length)
}

/// Writes a validated graph using absolute addresses relative to `base`.
/// Nodes contain kind, next sibling, payload, first child, count, and member key.
/// String payloads contain a data address and byte length; offset 16 is scalar length.
/// Word 28 is zero: the runtime marks objects whose members it has put in property order.
pub fn populate(input: &str, output: &mut [u8], base: u32) -> Result<u32, Error> {
    let mut parser = Parser {
        input,
        position: 0,
        output: Storage {
            bytes: output,
            length: 0,
            base,
            stores: true,
        },
    };
    let root = parser.document()?;
    parser.output.address(root)
}

struct Parser<'a> {
    input: &'a str,
    position: usize,
    output: Storage<'a>,
}

/// An array or object being parsed: its node, the last member linked, the member count and
/// (objects) the key node of the member being parsed.
#[derive(Clone, Copy)]
struct Open {
    node: usize,
    previous: Option<usize>,
    count: u32,
    key: usize,
    object: bool,
}

const CLOSED: Open = Open {
    node: 0,
    previous: None,
    count: 0,
    key: 0,
    object: false,
};

impl Parser<'_> {
    /// One loop over values with an explicit stack of open containers instead of recursion:
    /// the frame is fixed, so the linker can bound the stack of programs that parse JSON.
    /// Nodes are reserved in document order (a container, then each member's key and value).
    fn document(&mut self) -> Result<usize, Error> {
        let mut stack = [CLOSED; MAX_DEPTH];
        let mut depth = 0;
        loop {
            let (mut node, opened) = self.value(depth)?;
            if let Some(object) = opened {
                let end = if object { b'}' } else { b']' };
                self.whitespace();
                if self.peek() != Some(end) {
                    stack[depth] = Open {
                        node,
                        object,
                        ..CLOSED
                    };
                    depth += 1;
                    if object {
                        stack[depth - 1].key = self.key()?;
                    }
                    continue;
                }
                self.position += 1;
                self.word(node + 20, 0)?;
            }
            // `node` is complete: link it into its container, closing containers it ends.
            loop {
                let Some(open) = depth.checked_sub(1).map(|top| &mut stack[top]) else {
                    self.whitespace();
                    if self.position != self.input.len() {
                        return Err(Error::Syntax(self.position));
                    }
                    return Ok(node);
                };
                let open = *open;
                if open.object {
                    self.pointer(node + 24, open.key)?;
                }
                match open.previous {
                    Some(previous) => self.pointer(previous + 4, node)?,
                    None => self.pointer(open.node + 16, node)?,
                }
                let count = open.count.checked_add(1).ok_or(Error::Capacity)?;
                stack[depth - 1].previous = Some(node);
                stack[depth - 1].count = count;
                self.whitespace();
                let end = if open.object { b'}' } else { b']' };
                if self.peek() == Some(end) {
                    self.position += 1;
                    self.word(open.node + 20, count)?;
                    depth -= 1;
                    node = open.node;
                    continue;
                }
                self.expect(b',')?;
                if open.object {
                    stack[depth - 1].key = self.key()?;
                }
                break;
            }
        }
    }

    /// An object member's key and colon: the key's string node.
    fn key(&mut self) -> Result<usize, Error> {
        self.whitespace();
        let key = self.output.reserve(NODE_BYTES, 8)?;
        self.string(key)?;
        self.whitespace();
        self.expect(b':')?;
        Ok(key)
    }

    /// Parse a scalar value, or open an array or object (its kind written, the bracket
    /// consumed; [Parser::document] parses its members). Returns the node and, for a
    /// container, whether it is an object.
    fn value(&mut self, depth: usize) -> Result<(usize, Option<bool>), Error> {
        self.whitespace();
        if depth >= MAX_DEPTH {
            return Err(Error::Depth(self.position));
        }
        let node = self.output.reserve(NODE_BYTES, 8)?;
        self.word(node + 28, 0)?;
        match self.peek() {
            Some(b'n') => {
                self.keyword(b"null")?;
                self.word(node, Kind::Null as u32)?;
            }
            Some(b't') | Some(b'f') => {
                let truth = self.peek() == Some(b't');
                self.keyword(if truth { b"true" } else { b"false" })?;
                self.word(node, Kind::Boolean as u32)?;
                self.word(node + 8, u32::from(truth))?;
            }
            Some(b'"') => self.string(node)?,
            Some(b'[') | Some(b'{') => {
                let object = self.peek() == Some(b'{');
                self.position += 1;
                self.word(node, if object { Kind::Object } else { Kind::Array } as u32)?;
                return Ok((node, Some(object)));
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.position;
                if self.peek() == Some(b'-') {
                    self.position += 1;
                }
                if self.peek() == Some(b'0') {
                    self.position += 1;
                } else {
                    self.digits()?;
                }
                if self.peek() == Some(b'.') {
                    self.position += 1;
                    self.digits()?;
                }
                if matches!(self.peek(), Some(b'e' | b'E')) {
                    self.position += 1;
                    if matches!(self.peek(), Some(b'+' | b'-')) {
                        self.position += 1;
                    }
                    self.digits()?;
                }
                self.word(node, Kind::Number as u32)?;
                if self.output.stores {
                    let literal = &self.input.as_bytes()[start..self.position];
                    let value = super::number::parse_number(literal);
                    self.output.write(node + 8, &value.to_le_bytes())?;
                }
            }
            _ => return Err(Error::Syntax(self.position)),
        }
        Ok((node, None))
    }

    fn string(&mut self, node: usize) -> Result<(), Error> {
        self.expect(b'"')?;
        let start = self.output.reserve(0, 1)?;
        let mut byte_length = 0u32;
        let mut scalar_length = 0u32;
        loop {
            let scalar = match self.peek() {
                Some(b'"') => {
                    self.position += 1;
                    break;
                }
                Some(b'\\') => {
                    self.position += 1;
                    let escape = self.peek().ok_or(Error::Syntax(self.position))?;
                    self.position += 1;
                    match escape {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let at = self.position - 2;
                            let high = self.hex_quad()?;
                            let point = match high {
                                0xD800..=0xDBFF => {
                                    if self.input.as_bytes().get(self.position..self.position + 2)
                                        != Some(b"\\u")
                                    {
                                        return Err(Error::UnpairedSurrogate(at));
                                    }
                                    self.position += 2;
                                    let low = self.hex_quad()?;
                                    if !(0xDC00..=0xDFFF).contains(&low) {
                                        return Err(Error::UnpairedSurrogate(at));
                                    }
                                    0x10000 + ((high - 0xD800) << 10) + low - 0xDC00
                                }
                                0xDC00..=0xDFFF => return Err(Error::UnpairedSurrogate(at)),
                                point => point,
                            };
                            char::from_u32(point).ok_or(Error::Syntax(at))?
                        }
                        _ => return Err(Error::Syntax(self.position - 1)),
                    }
                }
                None | Some(0..=31) => return Err(Error::Syntax(self.position)),
                Some(_) => {
                    // A run of unescaped bytes, copied as is: the input is valid UTF-8.
                    let bytes = self.input.as_bytes();
                    let start = self.position;
                    while bytes
                        .get(self.position)
                        .is_some_and(|&b| b >= 0x20 && b != b'"' && b != b'\\')
                    {
                        self.position += 1;
                    }
                    let run = &bytes[start..self.position];
                    let destination = self.output.reserve(run.len(), 1)?;
                    self.output.write(destination, run)?;
                    byte_length = byte_length
                        .checked_add(run.len() as u32)
                        .ok_or(Error::Capacity)?;
                    let scalars = run.iter().filter(|&&b| b & 0xC0 != 0x80).count();
                    scalar_length = scalar_length
                        .checked_add(scalars as u32)
                        .ok_or(Error::Capacity)?;
                    continue;
                }
            };
            let mut bytes = [0; 4];
            let bytes = scalar.encode_utf8(&mut bytes).as_bytes();
            let destination = self.output.reserve(bytes.len(), 1)?;
            self.output.write(destination, bytes)?;
            byte_length = byte_length
                .checked_add(bytes.len() as u32)
                .ok_or(Error::Capacity)?;
            scalar_length = scalar_length.checked_add(1).ok_or(Error::Capacity)?;
        }
        self.word(node, Kind::String as u32)?;
        self.word(node + 28, 0)?;
        self.pointer(node + 8, start)?;
        self.word(node + 12, byte_length)?;
        self.word(node + 16, scalar_length)
    }

    fn hex_quad(&mut self) -> Result<u32, Error> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.peek() {
                Some(byte @ b'0'..=b'9') => u32::from(byte - b'0'),
                Some(byte @ b'a'..=b'f') => u32::from(byte - b'a') + 10,
                Some(byte @ b'A'..=b'F') => u32::from(byte - b'A') + 10,
                _ => return Err(Error::Syntax(self.position)),
            };
            self.position += 1;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn digits(&mut self) -> Result<(), Error> {
        let start = self.position;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.position += 1;
        }
        if self.position == start {
            return Err(Error::Syntax(start));
        }
        Ok(())
    }

    fn keyword(&mut self, keyword: &[u8]) -> Result<(), Error> {
        for &byte in keyword {
            self.expect(byte)?;
        }
        Ok(())
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }

    fn expect(&mut self, byte: u8) -> Result<(), Error> {
        if self.peek() != Some(byte) {
            return Err(Error::Syntax(self.position));
        }
        self.position += 1;
        Ok(())
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }

    fn word(&mut self, position: usize, word: u32) -> Result<(), Error> {
        self.output.write(position, &word.to_le_bytes())
    }

    fn pointer(&mut self, position: usize, target: usize) -> Result<(), Error> {
        self.word(position, self.output.address(target)?)
    }
}
