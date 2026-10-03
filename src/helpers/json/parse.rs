use super::{
    Error, Kind, MAX_DEPTH, NODE_BYTES,
    storage::{Buffer, Measure, Output},
};

/// Validates a document and computes its exact node/data storage requirement.
pub fn measure(input: &str) -> Result<usize, Error> {
    let mut parser = Parser {
        input,
        position: 0,
        output: Measure { length: 0 },
    };
    parser.document()?;
    Ok(parser.output.length)
}

/// Writes a validated graph using absolute addresses relative to `base`.
/// Nodes contain kind, next sibling, payload, first child, count, and member key.
/// String payloads contain a data address and byte length; offset 16 is scalar length.
pub fn populate(input: &str, output: &mut [u8], base: u32) -> Result<u32, Error> {
    let mut parser = Parser {
        input,
        position: 0,
        output: Buffer {
            bytes: output,
            length: 0,
            base,
        },
    };
    let root = parser.document()?;
    parser.output.address(root)
}

struct Parser<'a, W> {
    input: &'a str,
    position: usize,
    output: W,
}

impl<W: Output> Parser<'_, W> {
    fn document(&mut self) -> Result<usize, Error> {
        let root = self.value(0)?;
        self.whitespace();
        if self.position != self.input.len() {
            return Err(Error::Syntax(self.position));
        }
        Ok(root)
    }

    fn value(&mut self, depth: usize) -> Result<usize, Error> {
        self.whitespace();
        if depth >= MAX_DEPTH {
            return Err(Error::Depth(self.position));
        }
        let node = self.output.reserve(NODE_BYTES, 8)?;
        self.pointer(node + 28, 0)?;
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
                let end = if object { b'}' } else { b']' };
                let mut previous = None;
                let mut count = 0u32;
                self.whitespace();
                if self.peek() != Some(end) {
                    loop {
                        let key = if object {
                            self.whitespace();
                            let key = self.output.reserve(NODE_BYTES, 8)?;
                            self.string(key)?;
                            self.whitespace();
                            self.expect(b':')?;
                            Some(key)
                        } else {
                            None
                        };
                        let child = self.value(depth + 1)?;
                        if let Some(key) = key {
                            self.pointer(child + 24, key)?;
                        }
                        if let Some(previous) = previous {
                            self.pointer(previous + 4, child)?;
                        } else {
                            self.pointer(node + 16, child)?;
                        }
                        previous = Some(child);
                        count = count.checked_add(1).ok_or(Error::Capacity)?;
                        self.whitespace();
                        if self.peek() == Some(end) {
                            break;
                        }
                        self.expect(b',')?;
                    }
                }
                self.expect(end)?;
                self.word(node + 20, count)?;
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
                let value = self.input[start..self.position]
                    .parse::<f64>()
                    .map_err(|_| Error::Syntax(start))?;
                self.word(node, Kind::Number as u32)?;
                self.output.write(node + 8, &value.to_le_bytes())?;
            }
            _ => return Err(Error::Syntax(self.position)),
        }
        Ok(node)
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
                    let scalar = self.input[self.position..]
                        .chars()
                        .next()
                        .ok_or(Error::Syntax(self.position))?;
                    self.position += scalar.len_utf8();
                    scalar
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
        self.pointer(node + 28, 0)?;
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
