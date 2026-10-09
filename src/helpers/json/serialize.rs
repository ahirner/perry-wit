use super::{
    Error, Kind, MAX_DEPTH,
    storage::{Buffer, Measure, Output},
};

/// A read-only JSON tree. Child identities are stable within one serialization.
pub trait Tree {
    type Node: Copy + Eq;
    type Children: Iterator<Item = Result<Self::Node, Error>> + Clone;
    fn value(&self, node: Self::Node) -> Result<Value<'_, Self::Children>, Error>;
    fn key(&self, node: Self::Node) -> Result<&str, Error>;
}

/// Exactly one JSON variant; arrays and objects carry their child iterator.
pub enum Value<'a, C> {
    Undefined,
    Null,
    Boolean(bool),
    Number(f64),
    String(&'a str),
    Array(C),
    Object(C),
}

/// Measure compact ECMAScript serialization without allocating.
pub fn measure_tree<T: Tree>(tree: &T, root: T::Node) -> Result<usize, Error> {
    let mut output = Measure { length: 0 };
    write_tree(tree, root, &mut output, 0)?;
    Ok(output.length)
}

/// Write compact ECMAScript serialization into caller-owned storage.
pub fn serialize_tree<T: Tree>(tree: &T, root: T::Node, bytes: &mut [u8]) -> Result<usize, Error> {
    let mut output = Buffer {
        bytes,
        length: 0,
        base: 0,
    };
    write_tree(tree, root, &mut output, 0)?;
    Ok(output.length)
}

pub fn measure_serialized(bytes: &[u8], base: u32, root: u32) -> Result<usize, Error> {
    measure_tree(&Graph { bytes, base }, root)
}

pub fn serialize(bytes: &[u8], base: u32, root: u32, output: &mut [u8]) -> Result<usize, Error> {
    serialize_tree(&Graph { bytes, base }, root, output)
}

fn write_tree<T: Tree>(
    tree: &T,
    node: T::Node,
    output: &mut impl Output,
    depth: usize,
) -> Result<(), Error> {
    if depth >= MAX_DEPTH {
        return Err(Error::Depth(depth));
    }
    match tree.value(node)? {
        Value::Undefined | Value::Null => append(output, b"null"),
        Value::Boolean(value) => append(output, if value { b"true" } else { b"false" }),
        Value::Number(value) => {
            if value.is_finite() {
                append(
                    output,
                    ryu_js::Buffer::new().format_finite(value).as_bytes(),
                )
            } else {
                append(output, b"null")
            }
        }
        Value::String(value) => quote(value, output),
        Value::Array(children) => {
            append(output, b"[")?;
            for (index, child) in children.enumerate() {
                if index != 0 {
                    append(output, b",")?;
                }
                write_tree(tree, child?, output, depth + 1)?;
            }
            append(output, b"]")
        }
        Value::Object(children) => {
            append(output, b"{")?;
            let mut emitted = false;
            let mut previous_index = None;
            loop {
                let mut candidate = None;
                for child in children.clone() {
                    let child = child?;
                    if let Some(index) = index_key(tree.key(child)?)
                        && previous_index.is_none_or(|previous| index > previous)
                        && candidate.is_none_or(|(best, _)| index <= best)
                    {
                        candidate = Some((index, child));
                    }
                }
                let Some((index, child)) = candidate else {
                    break;
                };
                member(tree, child, output, depth, &mut emitted)?;
                previous_index = Some(index);
            }
            for child in children.clone() {
                let child = child?;
                let key = tree.key(child)?;
                if index_key(key).is_none() {
                    let mut first_match = None;
                    let mut last_match = None;
                    for scan in children.clone() {
                        let scan = scan?;
                        if tree.key(scan)? == key {
                            first_match.get_or_insert(scan);
                            last_match = Some(scan);
                        }
                    }
                    if first_match == Some(child) {
                        member(
                            tree,
                            last_match.ok_or(Error::InvalidGraph)?,
                            output,
                            depth,
                            &mut emitted,
                        )?;
                    }
                }
            }
            append(output, b"}")
        }
    }
}

fn member<T: Tree>(
    tree: &T,
    child: T::Node,
    output: &mut impl Output,
    depth: usize,
    emitted: &mut bool,
) -> Result<(), Error> {
    if matches!(tree.value(child)?, Value::Undefined) {
        return Ok(());
    }
    if *emitted {
        append(output, b",")?;
    }
    *emitted = true;
    quote(tree.key(child)?, output)?;
    append(output, b":")?;
    write_tree(tree, child, output, depth + 1)
}

fn index_key(key: &str) -> Option<u32> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    let mut value = 0u32;
    for byte in key.bytes() {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    (value != u32::MAX).then_some(value)
}

/// A checked view of the codec's flat graph; addresses use the supplied base.
#[derive(Clone, Copy)]
pub struct Graph<'a> {
    pub bytes: &'a [u8],
    pub base: u32,
}

#[derive(Clone)]
pub struct Children<'a> {
    graph: Graph<'a>,
    next: u32,
    remaining: Option<u32>,
}

impl Iterator for Children<'_> {
    type Item = Result<u32, Error>;
    fn next(&mut self) -> Option<Self::Item> {
        let remaining = self.remaining.take()?;
        if remaining == 0 {
            return (self.next != 0).then_some(Err(Error::InvalidGraph));
        }
        let current = self.next;
        match self.graph.word(current, 4) {
            Ok(next) if current != 0 => {
                self.next = next;
                self.remaining = Some(remaining - 1);
                Some(Ok(current))
            }
            _ => Some(Err(Error::InvalidGraph)),
        }
    }
}

impl<'a> Tree for Graph<'a> {
    type Node = u32;
    type Children = Children<'a>;
    fn value(&self, node: u32) -> Result<Value<'_, Children<'a>>, Error> {
        Ok(match self.word(node, 0)? {
            1 => Value::Null,
            2 => Value::Boolean(self.word(node, 8)? != 0),
            3 => Value::Number(f64::from_le_bytes(
                self.range(node.checked_add(8).ok_or(Error::InvalidGraph)?, 8)?
                    .try_into()
                    .map_err(|_| Error::InvalidGraph)?,
            )),
            4 => Value::String(self.string(node)?),
            kind @ (5 | 6) => {
                let children = Children {
                    graph: *self,
                    next: self.word(node, 16)?,
                    remaining: Some(self.word(node, 20)?),
                };
                if kind == 5 {
                    Value::Array(children)
                } else {
                    Value::Object(children)
                }
            }
            _ => return Err(Error::InvalidGraph),
        })
    }
    fn key(&self, node: u32) -> Result<&str, Error> {
        self.string(self.word(node, 24)?)
    }
}

impl Graph<'_> {
    fn string(&self, node: u32) -> Result<&str, Error> {
        if self.word(node, 0)? != Kind::String as u32 {
            return Err(Error::InvalidGraph);
        }
        core::str::from_utf8(self.range(self.word(node, 8)?, self.word(node, 12)? as usize)?)
            .map_err(|_| Error::InvalidGraph)
    }

    fn word(&self, node: u32, offset: u32) -> Result<u32, Error> {
        let at = node.checked_add(offset).ok_or(Error::InvalidGraph)?;
        Ok(u32::from_le_bytes(
            self.range(at, 4)?
                .try_into()
                .map_err(|_| Error::InvalidGraph)?,
        ))
    }

    fn range(&self, address: u32, length: usize) -> Result<&[u8], Error> {
        let start = address.checked_sub(self.base).ok_or(Error::InvalidGraph)? as usize;
        let end = start.checked_add(length).ok_or(Error::InvalidGraph)?;
        self.bytes.get(start..end).ok_or(Error::InvalidGraph)
    }
}

fn quote(text: &str, output: &mut impl Output) -> Result<(), Error> {
    append(output, b"\"")?;
    for scalar in text.chars() {
        match scalar {
            '"' => append(output, b"\\\"")?,
            '\\' => append(output, b"\\\\")?,
            '\u{8}' => append(output, b"\\b")?,
            '\u{c}' => append(output, b"\\f")?,
            '\n' => append(output, b"\\n")?,
            '\r' => append(output, b"\\r")?,
            '\t' => append(output, b"\\t")?,
            '\0'..='\u{1f}' => {
                let hex = b"0123456789abcdef";
                let code = scalar as usize;
                append(
                    output,
                    &[b'\\', b'u', b'0', b'0', hex[code >> 4], hex[code & 15]],
                )?;
            }
            scalar => {
                let mut bytes = [0; 4];
                append(output, scalar.encode_utf8(&mut bytes).as_bytes())?;
            }
        }
    }
    append(output, b"\"")
}

fn append(output: &mut impl Output, bytes: &[u8]) -> Result<(), Error> {
    let at = output.reserve(bytes.len(), 1)?;
    output.write(at, bytes)
}
