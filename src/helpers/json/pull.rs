//! Allocation-free, bounded-depth JSON pull tokenizer over valid UTF-8 input.
#[path = "number.rs"]
pub(super) mod number;
const MAX_DEPTH: u32 = 128;
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;
#[derive(Clone)]
pub struct Cursor<'a> {
    pub input: &'a [u8],
    pub len: u32,
    pub pos: u32,
    pub depth: u32,
    pub fresh: u32,
    pub broken: u32,
    objects: [u64; 2],
}
impl<'a> Cursor<'a> {
    pub fn new(text: &'a str) -> Self {
        Self {
            input: text.as_bytes(),
            len: text.len() as u32,
            pos: 0,
            depth: 0,
            fresh: 0,
            broken: 0,
            objects: [0; 2],
        }
    }
    pub fn byte(&self, at: u32) -> u8 {
        self.input.get(at as usize).copied().unwrap_or(0)
    }

    pub fn whitespace(&mut self) -> u8 {
        loop {
            let byte = self.byte(self.pos);
            match byte {
                b' ' | b'\n' | b'\r' | b'\t' => {
                    self.pos += 1;
                    // Indentation: eight spaces at a time.
                    while self.len - self.pos >= 8 && self.word(self.pos) == ONES * u64::from(b' ')
                    {
                        self.pos += 8;
                    }
                }
                _ => return byte,
            }
        }
    }

    fn word(&self, at: u32) -> u64 {
        u64::from_le_bytes(self.input[at as usize..at as usize + 8].try_into().unwrap())
    }

    pub fn push(&mut self, object: bool) -> bool {
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

    pub fn in_object(&self) -> bool {
        let top = self.depth - 1;
        self.objects[(top / 64) as usize] & (1 << (top % 64)) != 0
    }

    pub fn close(&mut self) {
        self.pos += 1;
        self.depth -= 1;
    }

    pub fn open(&mut self, bracket: u8) -> u32 {
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

    pub fn element(&mut self) -> u32 {
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

    pub fn skip(&mut self) {
        let base = self.depth;
        while self.broken == 0 {
            if self.enter() || self.after_value(base) {
                continue;
            }
            return;
        }
    }

    pub fn enter(&mut self) -> bool {
        match self.whitespace() {
            bracket @ (b'{' | b'[') => {
                let object = bracket == b'{';
                self.pos += 1;
                let end = if object { b'}' } else { b']' };
                if !self.push(object) {
                    return false;
                }
                if self.whitespace() == end {
                    self.close();
                    return false;
                }
                if object {
                    self.key();
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

    pub fn after_value(&mut self, base: u32) -> bool {
        while self.depth > base && self.broken == 0 {
            let object = self.in_object();
            match self.whitespace() {
                b',' => {
                    self.pos += 1;
                    if object {
                        self.key();
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

    pub fn key(&mut self) {
        if self.whitespace() != b'"' {
            self.broken = 1;
            return;
        }
        self.pos += 1;
        self.string();
        if self.whitespace() != b':' {
            self.broken = 1;
            return;
        }
        self.pos += 1;
    }

    pub fn literal(&mut self, word: &[u8]) -> bool {
        let end = self.pos as usize + word.len();
        if self.input.get(self.pos as usize..end) == Some(word) {
            self.pos = end as u32;
            true
        } else {
            self.broken = 1;
            false
        }
    }

    pub fn string(&mut self) -> (bool, u32) {
        let mut escaped = false;
        let mut continuations = 0;
        loop {
            // Eight bytes at a time to the first quote, backslash or control byte.
            while self.len - self.pos >= 8 {
                let word = self.word(self.pos);
                let quote = zero_bytes(word ^ (ONES * u64::from(b'"')));
                let backslash = zero_bytes(word ^ (ONES * u64::from(b'\\')));
                let control = word.wrapping_sub(ONES * 0x20) & !word & HIGHS;
                let special = quote | backslash | control;
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

    pub fn hex(&mut self) -> Option<u32> {
        let mut value = 0;
        for _ in 0..4 {
            let digit = match self.byte(self.pos) {
                byte @ b'0'..=b'9' => byte - b'0',
                byte @ b'a'..=b'f' => byte - b'a' + 10,
                byte @ b'A'..=b'F' => byte - b'A' + 10,
                _ => return None,
            };
            self.pos += 1;
            value = value * 16 + u32::from(digit);
        }
        Some(value)
    }

    pub fn number(&mut self) -> f64 {
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

    pub fn digits(&mut self) {
        let start = self.pos;
        while self.byte(self.pos).is_ascii_digit() {
            self.pos += 1;
        }
        if self.pos == start {
            self.broken = 1;
        }
    }
}

/// High bit set in the lowest zero byte of `word` (bits above it may be spurious).
#[inline(always)]
fn zero_bytes(word: u64) -> u64 {
    word.wrapping_sub(ONES) & !word & HIGHS
}
