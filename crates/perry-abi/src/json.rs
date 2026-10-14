//! Allocation-free JSON conventions shared by compiler passes and linked helpers.
pub mod kind {
    pub const UNDEFINED: u32 = 0;
    pub const NULL: u32 = 1;
    pub const BOOLEAN: u32 = 2;
    pub const NUMBER: u32 = 3;
    pub const STRING: u32 = 4;
    pub const ARRAY: u32 = 5;
    pub const OBJECT: u32 = 6;
    pub const EXCEPTION: u32 = 7;
    pub const NATIVE: u32 = 8;
}
pub const MAX_DEPTH: u32 = 128;
pub const MEMBER_COUNT: usize = 20;
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// The length of the leading run of `bytes` that needs no escape, eight bytes at a time.
pub fn unescaped_prefix(bytes: &[u8]) -> usize {
    let (chunks, tail) = bytes.as_chunks::<8>();
    for (index, &chunk) in chunks.iter().enumerate() {
        let special = special_bytes(u64::from_le_bytes(chunk));
        if special != 0 {
            return index * 8 + special.trailing_zeros() as usize / 8;
        }
    }
    bytes.len() - tail.len()
        + tail
            .iter()
            .position(|&byte| byte < 0x20 || byte == b'"' || byte == b'\\')
            .unwrap_or(tail.len())
}

/// Append the escaped contents of a JSON string literal, without its quotes.
pub fn quote_contents(mut bytes: &[u8], mut emit: impl FnMut(&[u8])) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    loop {
        let (plain, escaped) = bytes.split_at(unescaped_prefix(bytes));
        emit(plain);
        // The prefix ends at a byte that needs an escape, or at the end.
        let Some((&byte, rest)) = escaped.split_first() else {
            return;
        };
        let escape: &[u8] = match byte {
            b'"' => br#"\""#,
            b'\\' => br"\\",
            0x08 => br"\b",
            0x0C => br"\f",
            b'\n' => br"\n",
            b'\r' => br"\r",
            b'\t' => br"\t",
            _ => &[
                b'\\',
                b'u',
                b'0',
                b'0',
                HEX[usize::from(byte >> 4)],
                HEX[usize::from(byte & 15)],
            ],
        };
        emit(escape);
        bytes = rest;
    }
}

/// High bit set in the lowest quote, backslash or control byte of `word`: the bytes a JSON
/// string must escape (bits above the lowest may be spurious).
#[inline(always)]
fn special_bytes(word: u64) -> u64 {
    let quote = zero_bytes(word ^ (ONES * u64::from(b'"')));
    let backslash = zero_bytes(word ^ (ONES * u64::from(b'\\')));
    let control = word.wrapping_sub(ONES * 0x20) & !word & HIGHS;
    quote | backslash | control
}

/// High bit set in the lowest zero byte of `word` (bits above it may be spurious).
#[inline(always)]
fn zero_bytes(word: u64) -> u64 {
    word.wrapping_sub(ONES) & !word & HIGHS
}

/// Emit a JSON parser location using a zero-based byte offset and Unicode columns.
pub fn syntax_message(input: &[u8], offset: u32, mut emit: impl FnMut(&[u8])) {
    fn number(value: u32, emit: &mut impl FnMut(&[u8])) {
        let mut digits = [0u8; 10];
        let mut index = digits.len();
        let mut value = value;
        loop {
            index -= 1;
            digits[index] = b'0' + (value % 10) as u8;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        emit(&digits[index..]);
    }
    let prefix = &input[..(offset as usize).min(input.len())];
    let (mut line, mut column, mut carriage_return) = (1, 1, false);
    for &byte in prefix {
        match byte {
            b'\r' => {
                line += 1;
                column = 1;
                carriage_return = true;
            }
            b'\n' => {
                if !carriage_return {
                    line += 1;
                }
                column = 1;
                carriage_return = false;
            }
            _ => {
                if byte & 0xC0 != 0x80 {
                    column += 1;
                }
                carriage_return = false;
            }
        }
    }
    emit(b"Invalid JSON at byte ");
    number(offset, &mut emit);
    emit(b" (line ");
    number(line, &mut emit);
    emit(b", column ");
    number(column, &mut emit);
    emit(b")");
}

#[cfg(test)]
mod tests {
    use super::unescaped_prefix;

    #[test]
    fn prefixes_stop_at_escapes_at_word_boundaries() {
        for padding in 0..24 {
            let plain = format!("{}é漢🙂", "a".repeat(padding));
            assert_eq!(unescaped_prefix(plain.as_bytes()), plain.len());
            for stop in (0..32).chain(*b"\"\\") {
                let mut input = plain.as_bytes().to_vec();
                input.push(stop);
                input.extend_from_slice("é🙂".as_bytes());
                assert_eq!(unescaped_prefix(&input), plain.len());
            }
        }
        assert_eq!(unescaped_prefix(b""), 0);
    }
}
