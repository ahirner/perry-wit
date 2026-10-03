use super::{
    Error, Kind, MAX_DEPTH,
    storage::{Buffer, Measure, Output},
};

/// Measures compact ECMAScript serialization of a parsed graph.
pub fn measure_serialized(bytes: &[u8], base: u32, root: u32) -> Result<usize, Error> {
    let mut output = Measure { length: 0 };
    Graph { bytes, base }.serialize(root, &mut output, 0)?;
    Ok(output.length)
}

/// Serializes a parsed graph without allocating or modifying its values.
pub fn serialize(bytes: &[u8], base: u32, root: u32, output: &mut [u8]) -> Result<usize, Error> {
    let mut output = Buffer {
        bytes: output,
        length: 0,
        base: 0,
    };
    Graph { bytes, base }.serialize(root, &mut output, 0)?;
    Ok(output.length)
}

struct Graph<'a> {
    bytes: &'a [u8],
    base: u32,
}

impl Graph<'_> {
    fn serialize(&self, node: u32, output: &mut impl Output, depth: usize) -> Result<(), Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Depth(depth));
        }
        match self.word(node, 0)? {
            1 => append(output, b"null"),
            2 => append(
                output,
                if self.word(node, 8)? == 0 {
                    b"false"
                } else {
                    b"true"
                },
            ),
            3 => {
                let value = f64::from_le_bytes(
                    self.range(node.checked_add(8).ok_or(Error::InvalidGraph)?, 8)?
                        .try_into()
                        .map_err(|_| Error::InvalidGraph)?,
                );
                if value.is_finite() {
                    append(
                        output,
                        ryu_js::Buffer::new().format_finite(value).as_bytes(),
                    )
                } else {
                    append(output, b"null")
                }
            }
            4 => quote(self.string(node)?, output),
            5 => {
                append(output, b"[")?;
                let mut child = self.word(node, 16)?;
                for index in 0..self.word(node, 20)? {
                    if index != 0 {
                        append(output, b",")?;
                    }
                    self.serialize(child, output, depth + 1)?;
                    child = self.word(child, 4)?;
                }
                if child != 0 {
                    return Err(Error::InvalidGraph);
                }
                append(output, b"]")
            }
            6 => self.object(node, output, depth),
            _ => Err(Error::InvalidGraph),
        }
    }

    fn object(&self, node: u32, output: &mut impl Output, depth: usize) -> Result<(), Error> {
        append(output, b"{")?;
        let first = self.word(node, 16)?;
        let count = self.word(node, 20)?;
        let mut emitted = false;
        let mut previous_index = None;
        loop {
            let mut candidate = None;
            let mut child = first;
            for _ in 0..count {
                if let Some(index) = self.index_key(child)?
                    && previous_index.is_none_or(|previous| index > previous)
                    && candidate.is_none_or(|(best, _)| index <= best)
                {
                    candidate = Some((index, child));
                }
                child = self.word(child, 4)?;
            }
            let Some((index, child)) = candidate else {
                break;
            };
            self.member(child, output, depth, &mut emitted)?;
            previous_index = Some(index);
        }
        let mut child = first;
        for _ in 0..count {
            if self.index_key(child)?.is_none() {
                let key = self.string(self.word(child, 24)?)?;
                let mut scan = first;
                let mut first_match = None;
                let mut last_match = None;
                for _ in 0..count {
                    if self.string(self.word(scan, 24)?)? == key {
                        first_match.get_or_insert(scan);
                        last_match = Some(scan);
                    }
                    scan = self.word(scan, 4)?;
                }
                if first_match == Some(child) {
                    self.member(
                        last_match.ok_or(Error::InvalidGraph)?,
                        output,
                        depth,
                        &mut emitted,
                    )?;
                }
            }
            child = self.word(child, 4)?;
        }
        if child != 0 {
            return Err(Error::InvalidGraph);
        }
        append(output, b"}")
    }

    fn member(
        &self,
        child: u32,
        output: &mut impl Output,
        depth: usize,
        emitted: &mut bool,
    ) -> Result<(), Error> {
        if *emitted {
            append(output, b",")?;
        }
        *emitted = true;
        quote(self.string(self.word(child, 24)?)?, output)?;
        append(output, b":")?;
        self.serialize(child, output, depth + 1)
    }

    fn index_key(&self, node: u32) -> Result<Option<u32>, Error> {
        let key = self.string(self.word(node, 24)?)?;
        if key.is_empty() || (key.len() > 1 && key.starts_with('0')) {
            return Ok(None);
        }
        let mut value = 0u32;
        for byte in key.bytes() {
            if !byte.is_ascii_digit() {
                return Ok(None);
            }
            let Some(next) = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u32::from(byte - b'0')))
            else {
                return Ok(None);
            };
            value = next;
        }
        Ok((value != u32::MAX).then_some(value))
    }

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
