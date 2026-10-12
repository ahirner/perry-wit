//! Allocation-free JSON conventions shared by compiler passes and linked helpers.
pub mod kind {
    pub const NULL: u32 = 1;
    pub const BOOLEAN: u32 = 2;
    pub const NUMBER: u32 = 3;
    pub const STRING: u32 = 4;
    pub const ARRAY: u32 = 5;
    pub const OBJECT: u32 = 6;
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
