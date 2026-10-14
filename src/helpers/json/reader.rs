//! Shared UTF-8 JSON lexical operations; storage and schema construction stay in destinations.
use super::Error;

pub(super) struct Reader<'a> {
    pub(super) input: &'a str,
    pub(super) position: usize,
}

pub(super) struct StringFacts {
    pub(super) bytes: u32,
    pub(super) scalars: Option<u32>,
    #[cfg(target_arch = "wasm32")]
    pub(super) escaped: bool,
}

impl<'a> Reader<'a> {
    pub(super) fn new(input: &'a str) -> Self {
        Self { input, position: 0 }
    }
    pub(super) fn number(&mut self) -> Result<(), Error> {
        self.scan_number::<false>().map(|_| ())
    }
    pub(super) fn numeric(&mut self) -> Result<f64, Error> {
        let (decimal, start, end, negative, exponent) = self.scan_number::<true>()?;
        Ok(decimal.finish(&self.input.as_bytes()[start..end], negative, exponent))
    }
    #[cfg(target_arch = "wasm32")]
    pub(super) fn signed_integer(&mut self) -> Result<Option<i64>, Error> {
        let (decimal, start, end, negative, exponent) = self.scan_number::<true>()?;
        Ok(decimal.finish_i64(&self.input.as_bytes()[start..end], negative, exponent))
    }
    #[cfg(target_arch = "wasm32")]
    pub(super) fn unsigned_integer(&mut self) -> Result<Option<u64>, Error> {
        let (decimal, start, end, negative, exponent) = self.scan_number::<true>()?;
        Ok(decimal.finish_u64(&self.input.as_bytes()[start..end], negative, exponent))
    }
    fn scan_number<const CONVERT: bool>(
        &mut self,
    ) -> Result<(super::number::Decimal, usize, usize, bool, i64), Error> {
        let negative = self.peek() == Some(b'-');
        if negative {
            self.position += 1;
        }
        let start = self.position;
        let mut decimal = super::number::Decimal::default();
        if self.peek() == Some(b'0') {
            self.position += 1;
            if CONVERT {
                decimal.digit(b'0', false);
            }
        } else {
            self.digits(|byte| {
                if CONVERT {
                    decimal.digit(byte, false)
                }
            })?;
        }
        if self.peek() == Some(b'.') {
            self.position += 1;
            self.digits(|byte| {
                if CONVERT {
                    decimal.digit(byte, true)
                }
            })?;
        }
        let end = self.position;
        let mut exponent = 0i64;
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.position += 1;
            let negative = self.peek() == Some(b'-');
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.position += 1;
            }
            self.digits(|byte| {
                if CONVERT {
                    exponent = (exponent * 10 + i64::from(byte - b'0')).min(1 << 40)
                }
            })?;
            if negative {
                exponent = -exponent;
            }
        }
        Ok((decimal, start, end, negative, exponent))
    }

    pub(super) fn string<const COUNT: bool>(
        &mut self,
        mut emit: impl FnMut(&[u8]) -> Result<(), Error>,
    ) -> Result<StringFacts, Error> {
        self.expect(b'"')?;
        let mut byte_length = 0u32;
        let mut scalar_length = 0u32;
        #[cfg(target_arch = "wasm32")]
        let mut escaped = false;
        loop {
            let scalar = match self.peek() {
                Some(b'"') => {
                    self.position += 1;
                    break;
                }
                Some(b'\\') => {
                    #[cfg(target_arch = "wasm32")]
                    {
                        escaped = true;
                    }
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
                    self.position += perry_abi::json::unescaped_prefix(&bytes[self.position..]);
                    let run = &bytes[start..self.position];
                    emit(run)?;
                    byte_length = byte_length
                        .checked_add(run.len() as u32)
                        .ok_or(Error::Capacity)?;
                    if COUNT {
                        scalar_length = scalar_length
                            .checked_add(perry_abi::utf8::scalars_length_wide(run))
                            .ok_or(Error::Capacity)?;
                    }
                    continue;
                }
            };
            let mut bytes = [0; 4];
            let bytes = scalar.encode_utf8(&mut bytes).as_bytes();
            emit(bytes)?;
            byte_length = byte_length
                .checked_add(bytes.len() as u32)
                .ok_or(Error::Capacity)?;
            scalar_length = scalar_length.checked_add(1).ok_or(Error::Capacity)?;
        }
        Ok(StringFacts {
            bytes: byte_length,
            scalars: COUNT.then_some(scalar_length),
            #[cfg(target_arch = "wasm32")]
            escaped,
        })
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

    fn digits(&mut self, mut digit: impl FnMut(u8)) -> Result<(), Error> {
        let start = self.position;
        while let Some(byte @ b'0'..=b'9') = self.peek() {
            digit(byte);
            self.position += 1;
        }
        if self.position == start {
            return Err(Error::Syntax(start));
        }
        Ok(())
    }

    pub(super) fn keyword(&mut self, keyword: &[u8]) -> Result<(), Error> {
        for &byte in keyword {
            self.expect(byte)?;
        }
        Ok(())
    }

    pub(super) fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.position += 1;
        }
    }

    pub(super) fn expect(&mut self, byte: u8) -> Result<(), Error> {
        if self.peek() != Some(byte) {
            return Err(Error::Syntax(self.position));
        }
        self.position += 1;
        Ok(())
    }

    pub(super) fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.position).copied()
    }
}
