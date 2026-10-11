//! Allocation-free, bounded-depth JSON pull tokenizer over valid UTF-8 input.
//!
//! Typed `JSON.parse` in `perry-std` (`json_read`, `json_source`) drives it directly; errors
//! latch `broken`, after which every read stops so generated decoders unwind.
#[path = "number.rs"]
mod number;
use perry_abi::json::{MAX_DEPTH, special_bytes, word};
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// Position and open-container state of one pass over a JSON text.
pub(crate) struct Cursor<'a> {
    pub(crate) input: &'a [u8],
    pub(crate) len: u32,
    pub(crate) pos: u32,
    pub(crate) depth: u32,
    /// Nonzero right after `open`: the next element or member takes no separator.
    fresh: u32,
    /// Nonzero once the text proved malformed (or too deep).
    pub(crate) broken: u32,
    /// Bit `d` set: the container at depth `d` is an object.
    objects: [u64; 2],
}
impl<'a> Cursor<'a> {
    /// A cursor at the start of `text`, the UTF-8 bytes of a string.
    pub(crate) fn new(text: &'a [u8]) -> Self {
        Self::over(text, 0)
    }

    /// A fresh cursor over the same (already validated) input, positioned at `pos`.
    pub(crate) fn at(&self, pos: u32) -> Self {
        Self::over(self.input, pos)
    }

    fn over(input: &'a [u8], pos: u32) -> Self {
        Self {
            input,
            len: input.len() as u32,
            pos,
            depth: 0,
            fresh: 0,
            broken: 0,
            objects: [0; 2],
        }
    }

    fn byte(&self, at: u32) -> u8 {
        self.input.get(at as usize).copied().unwrap_or(0)
    }

    /// Skip whitespace; returns the next byte, or 0 at the end of the text.
    pub(crate) fn whitespace(&mut self) -> u8 {
        loop {
            let byte = self.byte(self.pos);
            match byte {
                b' ' | b'\n' | b'\r' | b'\t' => {
                    self.pos += 1;
                    // Indentation: eight spaces at a time.
                    while word(self.input, self.pos as usize) == Some(ONES * u64::from(b' ')) {
                        self.pos += 8;
                    }
                }
                _ => return byte,
            }
        }
    }

    /// Enter a container (`object` or array); false, breaking the text, past [MAX_DEPTH].
    pub(crate) fn push(&mut self, object: bool) -> bool {
        if self.depth >= MAX_DEPTH {
            self.broken = 1;
            return false;
        }
        let (word, bit) = ((self.depth / 64) as usize, 1u64 << (self.depth % 64));
        if object {
            self.objects[word] |= bit;
        } else {
            self.objects[word] &= !bit;
        }
        self.depth += 1;
        true
    }

    fn in_object(&self) -> bool {
        let top = self.depth - 1;
        self.objects[(top / 64) as usize] & (1 << (top % 64)) != 0
    }

    fn close(&mut self) {
        self.pos += 1;
        self.depth -= 1;
    }

    /// Enter the next value when it opens with `bracket`: 1, or 0 after skipping a value
    /// of another kind (or on a broken text).
    pub(crate) fn open(&mut self, bracket: u8) -> u32 {
        if self.broken != 0 {
            return 0;
        }
        if self.whitespace() != bracket {
            self.skip();
            return 0;
        }
        self.pos += 1;
        if !self.push(bracket == b'{') {
            return 0;
        }
        self.fresh = 1;
        1
    }

    /// Advance to the next element of the innermost array: 1, or 0 when it ended.
    pub(crate) fn element(&mut self) -> u32 {
        if self.broken != 0 {
            return 0;
        }
        let next = self.whitespace();
        if core::mem::take(&mut self.fresh) != 0 {
            if next == b']' {
                self.close();
                return 0;
            }
            return 1;
        }
        match next {
            b',' => {
                self.pos += 1;
                1
            }
            b']' => {
                self.close();
                0
            }
            _ => {
                self.broken = 1;
                0
            }
        }
    }

    /// Read an object key, leaving the cursor at its value. The flag marks escaped keys.
    #[inline]
    pub(crate) fn member(&mut self) -> Option<(&'a [u8], bool)> {
        if self.broken != 0 {
            return None;
        }
        let mut next = self.whitespace();
        if core::mem::take(&mut self.fresh) == 0 {
            match next {
                b',' => {
                    self.pos += 1;
                    next = self.whitespace();
                }
                b'}' => {
                    self.close();
                    return None;
                }
                _ => {
                    self.broken = 1;
                    return None;
                }
            }
        } else if next == b'}' {
            self.close();
            return None;
        }
        self.key(next)
    }

    /// Validate and pass the next value, iteratively: the stack stays bounded.
    pub(crate) fn skip(&mut self) {
        let base = self.depth;
        while self.broken == 0 && (self.enter() || self.after_value(base)) {}
    }

    /// Pass a scalar, or enter a non-empty container: true when positioned inside one.
    fn enter(&mut self) -> bool {
        match self.whitespace() {
            bracket @ (b'{' | b'[') => {
                let object = bracket == b'{';
                self.pos += 1;
                let end = if object { b'}' } else { b']' };
                if !self.push(object) {
                    return false;
                }
                let next = self.whitespace();
                if next == end {
                    self.close();
                    return false;
                }
                if object {
                    let _ = self.key(next);
                }
                true
            }
            b'"' => {
                self.pos += 1;
                self.string();
                false
            }
            b'-' | b'0'..=b'9' => {
                self.scan_number::<false>();
                false
            }
            first @ (b't' | b'f' | b'n') => {
                self.literal(match first {
                    b't' => b"true",
                    b'f' => b"false",
                    _ => b"null",
                });
                false
            }
            _ => {
                self.broken = 1;
                false
            }
        }
    }

    /// Close containers above `base` that end here: true when another element follows.
    fn after_value(&mut self, base: u32) -> bool {
        while self.depth > base && self.broken == 0 {
            let object = self.in_object();
            match self.whitespace() {
                b',' => {
                    self.pos += 1;
                    if object {
                        let next = self.whitespace();
                        let _ = self.key(next);
                    }
                    return true;
                }
                b'}' if object => self.close(),
                b']' if !object => self.close(),
                _ => self.broken = 1,
            }
        }
        false
    }

    #[inline]
    fn key(&mut self, next: u8) -> Option<(&'a [u8], bool)> {
        if next != b'"' {
            self.broken = 1;
            return None;
        }
        self.pos += 1;
        let start = self.pos as usize;
        let (escaped, _) = self.string();
        let end = self.pos as usize - 1;
        if self.whitespace() != b':' || self.broken != 0 {
            self.broken = 1;
            return None;
        }
        self.pos += 1;
        Some((&self.input[start..end], escaped))
    }

    /// Pass `word` (`true`, `false`, `null`), or break the text when it does not follow.
    pub(crate) fn literal(&mut self, word: &[u8]) -> bool {
        let end = self.pos as usize + word.len();
        if self.input.get(self.pos as usize..end) == Some(word) {
            self.pos = end as u32;
            true
        } else {
            self.broken = 1;
            false
        }
    }

    /// Validate string content after its opening quote, through the closing one. Returns
    /// whether it has escapes, and its count of UTF-8 continuation bytes.
    pub(crate) fn string(&mut self) -> (bool, u32) {
        let mut escaped = false;
        let mut continuations = 0;
        loop {
            // Eight bytes at a time to the first quote, backslash or control byte.
            while let Some(word) = word(self.input, self.pos as usize) {
                let special = special_bytes(word);
                // `10xxxxxx`: the high bit set and the next one clear.
                let continuation = word & HIGHS & !(word << 1);
                if special != 0 {
                    let bytes = special.trailing_zeros() / 8;
                    let before = (1u64 << (bytes * 8)) - 1;
                    continuations += (continuation & before).count_ones();
                    self.pos += bytes;
                    break;
                }
                continuations += continuation.count_ones();
                self.pos += 8;
            }
            match self.byte(self.pos) {
                b'"' => {
                    self.pos += 1;
                    return (escaped, continuations);
                }
                b'\\' => {
                    escaped = true;
                    self.pos += 1;
                    match self.byte(self.pos) {
                        b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => self.pos += 1,
                        b'u' => {
                            self.pos += 1;
                            let Some(high) = self.hex() else {
                                self.broken = 1;
                                return (escaped, continuations);
                            };
                            match high {
                                0xD800..=0xDBFF => {
                                    let paired = self.byte(self.pos) == b'\\'
                                        && self.byte(self.pos + 1) == b'u'
                                        && {
                                            self.pos += 2;
                                            self.hex()
                                                .is_some_and(|low| (0xDC00..=0xDFFF).contains(&low))
                                        };
                                    if !paired {
                                        self.broken = 1;
                                        return (escaped, continuations);
                                    }
                                }
                                0xDC00..=0xDFFF => {
                                    self.broken = 1;
                                    return (escaped, continuations);
                                }
                                _ => {}
                            }
                        }
                        _ => {
                            self.broken = 1;
                            return (escaped, continuations);
                        }
                    }
                }
                0..=0x1F => {
                    // A control byte, or the end of the text.
                    self.broken = 1;
                    return (escaped, continuations);
                }
                _ => {
                    continuations += u32::from(self.byte(self.pos) & 0xC0 == 0x80);
                    self.pos += 1;
                }
            }
        }
    }

    fn hex(&mut self) -> Option<u32> {
        let (value, _) = hex4(self.input.get(self.pos as usize..)?)?;
        self.pos += 4;
        Some(value)
    }

    /// Validate and convert the number at the cursor.
    pub(crate) fn number(&mut self) -> f64 {
        self.scan_number::<true>()
    }

    fn scan_number<const CONVERT: bool>(&mut self) -> f64 {
        let start = self.pos;
        if self.byte(self.pos) == b'-' {
            self.pos += 1;
        }
        // Integers of up to 15 digits convert exactly as they are read.
        let mut integer = 0u64;
        let digits_start = self.pos;
        if self.byte(self.pos) == b'0' {
            self.pos += 1;
        } else {
            while let digit @ b'0'..=b'9' = self.byte(self.pos) {
                if CONVERT {
                    integer = integer
                        .wrapping_mul(10)
                        .wrapping_add(u64::from(digit - b'0'));
                }
                self.pos += 1;
            }
            if self.pos == digits_start {
                self.broken = 1;
                return 0.0;
            }
        }
        let integral_digits = self.pos - digits_start;
        let mut simple = integral_digits <= 15;
        if self.byte(self.pos) == b'.' {
            self.pos += 1;
            simple = false;
            self.digits();
        }
        if matches!(self.byte(self.pos), b'e' | b'E') {
            self.pos += 1;
            simple = false;
            if matches!(self.byte(self.pos), b'+' | b'-') {
                self.pos += 1;
            }
            self.digits();
        }
        if !CONVERT || self.broken != 0 {
            return 0.0;
        }
        let negative = self.byte(start) == b'-';
        if simple {
            let magnitude = integer as f64;
            return if negative { -magnitude } else { magnitude };
        }
        let literal = &self.input[start as usize..self.pos as usize];
        number::parse_number(literal)
    }

    fn digits(&mut self) {
        let start = self.pos;
        while self.byte(self.pos).is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos == start {
            self.broken = 1;
        }
    }
}

/// The value of the four hex digits `input` starts with, if it does, and the bytes after them.
pub(crate) fn hex4(input: &[u8]) -> Option<(u32, &[u8])> {
    let (digits, rest) = input.split_first_chunk::<4>()?;
    let value = digits.iter().try_fold(0, |value, &digit| {
        Some(value * 16 + char::from(digit).to_digit(16)?)
    })?;
    Some((value, rest))
}
