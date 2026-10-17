use super::reader::Reader;
use super::{
    Error, Kind, MAX_DEPTH, NODE_BYTES,
    storage::{GraphStorage, Storage},
};
use arrayvec::ArrayVec;
use perry_abi::json::MEMBER_COUNT;

/// Validates a document and computes its exact node/data storage requirement.
pub fn measure(input: &str) -> Result<usize, Error> {
    let mut parser = Parser {
        reader: Reader::new(input),
        initial_depth: 0,
        output: Storage::<false> {
            bytes: &mut [],
            length: 0,
            base: 0,
        },
    };
    parser.document(true)?;
    Ok(parser.output.length)
}

/// Writes a validated graph using absolute addresses relative to `base`.
/// Nodes contain kind, next sibling, payload, first child, count, and member key.
/// String payloads contain a data address and byte length; offset 16 is scalar length.
/// Word 28 is zero: the runtime marks objects whose members it has put in property order.
pub fn populate(input: &str, output: &mut [u8], base: u32) -> Result<u32, Error> {
    let mut parser = Parser {
        reader: Reader::new(input),
        initial_depth: 0,
        output: Storage::<true> {
            bytes: output,
            length: 0,
            base,
        },
    };
    let root = parser.document(true)?;
    parser.output.address(root)
}

#[cfg(target_arch = "wasm32")]
pub(super) fn skip(input: &str, start: usize, initial_depth: usize) -> Result<usize, Error> {
    let mut reader = Reader::new(input);
    reader.position = start;
    let mut parser = Parser {
        reader,
        initial_depth,
        output: Storage::<false> {
            bytes: &mut [],
            length: 0,
            base: 0,
        },
    };
    parser.document(false)?;
    Ok(parser.reader.position)
}

struct Parser<'a, S> {
    reader: Reader<'a>,
    initial_depth: usize,
    output: S,
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

impl<S: GraphStorage> Parser<'_, S> {
    /// One loop over values with an explicit stack of open containers instead of recursion:
    /// the frame is fixed, so the linker can bound the stack of programs that parse JSON.
    /// Nodes are reserved in document order (a container, then each member's key and value).
    fn document(&mut self, complete: bool) -> Result<usize, Error> {
        let mut stack = ArrayVec::<Open, MAX_DEPTH>::new();
        loop {
            let (mut node, opened) = self.value(self.initial_depth + stack.len())?;
            if let Some(object) = opened {
                let end = if object { b'}' } else { b']' };
                self.reader.whitespace();
                if self.reader.peek() != Some(end) {
                    stack.push(Open {
                        node,
                        object,
                        previous: None,
                        count: 0,
                        key: if object { self.key()? } else { 0 },
                    });
                    continue;
                }
                self.reader.position += 1;
                self.word(node + MEMBER_COUNT, 0)?;
            }
            // `node` is complete: link it into its container, closing containers it ends.
            loop {
                let Some(open) = stack.last_mut() else {
                    self.reader.whitespace();
                    if complete && self.reader.position != self.reader.input.len() {
                        return Err(Error::Syntax(self.reader.position));
                    }
                    return Ok(node);
                };
                if open.object {
                    self.pointer(node + 24, open.key)?;
                }
                match open.previous {
                    Some(previous) => self.pointer(previous + 4, node)?,
                    None => self.pointer(open.node + 16, node)?,
                }
                let count = open.count.checked_add(1).ok_or(Error::Capacity)?;
                open.previous = Some(node);
                open.count = count;
                self.reader.whitespace();
                let end = if open.object { b'}' } else { b']' };
                if self.reader.peek() == Some(end) {
                    self.reader.position += 1;
                    self.word(open.node + MEMBER_COUNT, count)?;
                    node = open.node;
                    stack.pop();
                    continue;
                }
                self.reader.expect(b',')?;
                if open.object {
                    open.key = self.key()?;
                }
                break;
            }
        }
    }

    /// An object member's key and colon: the key's string node.
    fn key(&mut self) -> Result<usize, Error> {
        self.reader.whitespace();
        let key = self.output.string(&mut self.reader)?;
        self.reader.whitespace();
        self.reader.expect(b':')?;
        Ok(key)
    }

    /// Parse a scalar value, or open an array or object (its kind written, the bracket
    /// consumed; [Parser::document] parses its members). Returns the node and, for a
    /// container, whether it is an object.
    fn value(&mut self, depth: usize) -> Result<(usize, Option<bool>), Error> {
        self.reader.whitespace();
        if depth >= MAX_DEPTH {
            return Err(Error::Depth(self.reader.position));
        }
        if self.reader.peek() == Some(b'"') {
            return Ok((self.output.string(&mut self.reader)?, None));
        }
        let node = self.output.reserve(NODE_BYTES, 8)?;
        self.word(node + 28, 0)?;
        match self.reader.peek() {
            Some(b'n') => {
                self.reader.keyword(b"null")?;
                self.word(node, Kind::Null as u32)?;
            }
            Some(b't') | Some(b'f') => {
                let truth = self.reader.peek() == Some(b't');
                self.reader
                    .keyword(if truth { b"true" } else { b"false" })?;
                self.word(node, Kind::Boolean as u32)?;
                self.word(node + 8, u32::from(truth))?;
            }
            Some(b'[') | Some(b'{') => {
                let object = self.reader.peek() == Some(b'{');
                self.reader.position += 1;
                self.word(node, if object { Kind::Object } else { Kind::Array } as u32)?;
                return Ok((node, Some(object)));
            }
            Some(b'-' | b'0'..=b'9') => {
                self.word(node, Kind::Number as u32)?;
                if S::STORE {
                    let value = self.reader.numeric()?;
                    self.output.write(node + 8, &value.to_le_bytes())?;
                } else {
                    self.reader.number()?;
                }
            }
            _ => return Err(Error::Syntax(self.reader.position)),
        }
        Ok((node, None))
    }

    fn word(&mut self, position: usize, word: u32) -> Result<(), Error> {
        self.output.write(position, &word.to_le_bytes())
    }

    fn pointer(&mut self, position: usize, target: usize) -> Result<(), Error> {
        self.word(position, self.output.address(target)?)
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) fn pooled(
    input: &str,
    start: usize,
    complete: bool,
    initial_depth: usize,
    residue: &mut super::pool::Residue,
) -> Result<(u32, usize), Error> {
    let mut reader = Reader::new(input);
    reader.position = start;
    let mut parser = Parser {
        reader,
        initial_depth,
        output: super::pool::Pooled(residue),
    };
    let root = parser.document(complete)?;
    Ok((parser.output.address(root)?, parser.reader.position))
}
