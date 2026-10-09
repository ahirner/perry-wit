use super::{
    Error, Kind, MAX_DEPTH,
    storage::{Buffer, Measure, Output},
};

/// A read-only JSON tree. Child identities are stable within one serialization.
pub trait Tree {
    type Node: Copy + Eq;
    type Identity: Copy + Eq;
    type Children: Iterator<Item = Result<Self::Node, Error>> + Clone;
    /// Storage identity of an array or object, shared by all aliases of that container.
    fn identity(&self, node: Self::Node) -> Result<Self::Identity, Error>;
    fn value(&self, node: Self::Node) -> Result<Value<'_, Self::Children>, Error>;
    fn key(&self, node: Self::Node) -> Result<&str, Error>;

    /// Find a property without visiting its value. Flat graphs may contain duplicate keys.
    fn property(&self, node: Self::Node, key: &str) -> Result<Option<Self::Node>, Error> {
        let Value::Object(children) = self.value(node)? else {
            return Ok(None);
        };
        let mut found = None;
        for child in children {
            let child = child?;
            if self.key(child)? == key {
                found = Some(child);
            }
        }
        Ok(found)
    }
}

/// Prepared serialization controls. Property names are unique and retain replacer order.
pub struct StringifyOptions<'a> {
    /// A representation error matters only when indentation is actually emitted.
    pub gap: Result<&'a str, Error>,
    pub properties: Option<&'a [&'a str]>,
}
impl Default for StringifyOptions<'_> {
    fn default() -> Self {
        Self {
            gap: Ok(""),
            properties: None,
        }
    }
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
    measure_tree_with_options(tree, root, &StringifyOptions::default())
}

pub fn measure_tree_with_options<T: Tree>(
    tree: &T,
    root: T::Node,
    options: &StringifyOptions<'_>,
) -> Result<usize, Error> {
    let mut output = Measure { length: 0 };
    write_tree(tree, root, &mut output, options)?;
    Ok(output.length)
}

/// Write compact ECMAScript serialization into caller-owned storage.
pub fn serialize_tree<T: Tree>(tree: &T, root: T::Node, bytes: &mut [u8]) -> Result<usize, Error> {
    serialize_tree_with_options(tree, root, bytes, &StringifyOptions::default())
}

pub fn serialize_tree_with_options<T: Tree>(
    tree: &T,
    root: T::Node,
    bytes: &mut [u8],
    options: &StringifyOptions<'_>,
) -> Result<usize, Error> {
    let mut output = Buffer {
        bytes,
        length: 0,
        base: 0,
    };
    write_tree(tree, root, &mut output, options)?;
    Ok(output.length)
}

pub fn measure_serialized(bytes: &[u8], base: u32, root: u32) -> Result<usize, Error> {
    measure_tree(&Graph { bytes, base }, root)
}

pub fn serialize(bytes: &[u8], base: u32, root: u32, output: &mut [u8]) -> Result<usize, Error> {
    serialize_tree(&Graph { bytes, base }, root, output)
}

enum Members<C> {
    Array(C),
    Object { all: C, order: ObjectOrder<C> },
}

enum ObjectOrder<C> {
    Filter(usize),
    Numeric(Option<u32>),
    Named(C),
}

struct Frame<T: Tree> {
    node: T::Node,
    identity: T::Identity,
    members: Members<T::Children>,
    emitted: bool,
}

impl<T: Tree> Frame<T> {
    fn next(&mut self, tree: &T, properties: Option<&[&str]>) -> Result<Option<T::Node>, Error> {
        let (all, order) = match &mut self.members {
            Members::Array(children) => return children.next().transpose(),
            Members::Object { all, order } => (all, order),
        };
        loop {
            match order {
                ObjectOrder::Filter(index) => {
                    let Some(key) = properties.expect("filtered property order").get(*index) else {
                        return Ok(None);
                    };
                    *index += 1;
                    if let Some(child) = tree.property(self.node, key)? {
                        return Ok(Some(child));
                    }
                }
                ObjectOrder::Numeric(previous) => {
                    let mut candidate = None;
                    for child in all.clone() {
                        let child = child?;
                        if let Some(index) = index_key(tree.key(child)?)
                            && previous.is_none_or(|previous| index > previous)
                            && candidate.is_none_or(|(best, _)| index <= best)
                        {
                            candidate = Some((index, child));
                        }
                    }
                    if let Some((index, child)) = candidate {
                        *previous = Some(index);
                        return Ok(Some(child));
                    }
                    *order = ObjectOrder::Named(all.clone());
                }
                ObjectOrder::Named(children) => {
                    let Some(child) = children.next() else {
                        return Ok(None);
                    };
                    let child = child?;
                    let key = tree.key(child)?;
                    if index_key(key).is_some() {
                        continue;
                    }
                    let mut first = None;
                    let mut last = None;
                    for scan in all.clone() {
                        let scan = scan?;
                        if tree.key(scan)? == key {
                            first.get_or_insert(scan);
                            last = Some(scan);
                        }
                    }
                    if first == Some(child) {
                        return Ok(last);
                    }
                }
            }
        }
    }
}

fn write_tree<T: Tree>(
    tree: &T,
    root: T::Node,
    output: &mut impl Output,
    options: &StringifyOptions<'_>,
) -> Result<(), Error> {
    let mut frames: [Option<Frame<T>>; MAX_DEPTH] = [const { None }; MAX_DEPTH];
    let mut depth = 0;
    let mut pending = Some(root);
    loop {
        if let Some(node) = pending.take() {
            if depth >= MAX_DEPTH {
                return Err(Error::Depth(depth));
            }
            let container = match tree.value(node)? {
                Value::Undefined | Value::Null => {
                    append(output, b"null")?;
                    None
                }
                Value::Boolean(value) => {
                    append(output, if value { b"true" } else { b"false" })?;
                    None
                }
                Value::Number(value) => {
                    if value.is_finite() {
                        append(
                            output,
                            ryu_js::Buffer::new().format_finite(value).as_bytes(),
                        )?;
                    } else {
                        append(output, b"null")?;
                    }
                    None
                }
                Value::String(value) => {
                    quote(value, output)?;
                    None
                }
                Value::Array(children) => Some(Members::Array(children)),
                Value::Object(all) => Some(Members::Object {
                    all,
                    order: if options.properties.is_some() {
                        ObjectOrder::Filter(0)
                    } else {
                        ObjectOrder::Numeric(None)
                    },
                }),
            };
            if let Some(members) = container {
                let identity = tree.identity(node)?;
                if frames[..depth].iter().any(|frame| {
                    frame
                        .as_ref()
                        .is_some_and(|frame| frame.identity == identity)
                }) {
                    return Err(Error::Cycle);
                }
                append(
                    output,
                    if matches!(members, Members::Array(_)) {
                        b"["
                    } else {
                        b"{"
                    },
                )?;
                frames[depth] = Some(Frame {
                    node,
                    identity,
                    members,
                    emitted: false,
                });
                depth += 1;
            }
        }
        while depth > 0 {
            let frame = frames[depth - 1].as_mut().expect("live traversal frame");
            let object = matches!(frame.members, Members::Object { .. });
            if let Some(child) = frame.next(tree, options.properties)? {
                if object && matches!(tree.value(child)?, Value::Undefined) {
                    continue;
                }
                if frame.emitted {
                    append(output, b",")?;
                }
                line(output, options.gap, depth)?;
                if object {
                    quote(tree.key(child)?, output)?;
                    append(output, if options.gap?.is_empty() { b":" } else { b": " })?;
                }
                frame.emitted = true;
                pending = Some(child);
                break;
            }
            if frame.emitted {
                line(output, options.gap, depth - 1)?;
            }
            append(output, if object { b"}" } else { b"]" })?;
            frames[depth - 1] = None;
            depth -= 1;
        }
        if pending.is_none() {
            return Ok(());
        }
    }
}

fn line(output: &mut impl Output, gap: Result<&str, Error>, depth: usize) -> Result<(), Error> {
    let gap = gap?;
    if !gap.is_empty() {
        append(output, b"\n")?;
        for _ in 0..depth {
            append(output, gap.as_bytes())?;
        }
    }
    Ok(())
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
    type Identity = u32;
    type Children = Children<'a>;
    fn identity(&self, node: u32) -> Result<u32, Error> {
        Ok(node)
    }
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
