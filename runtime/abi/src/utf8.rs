//! Byte scans shared by the instance runtime and standard libraries.

/// Compact scalar counter for minimal runtime components.
pub fn scalars_length(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|&byte| u32::from(byte & 0xc0 != 0x80))
        .sum()
}

/// Eight-byte scalar counter for bulk UTF-8 input and output.
#[inline]
pub fn scalars_length_wide(bytes: &[u8]) -> u32 {
    let (words, tail) = bytes.as_chunks::<8>();
    let mut continuations = 0;
    for word in words {
        let word = u64::from_le_bytes(*word);
        continuations += (word & !(word << 1) & 0x8080_8080_8080_8080).count_ones();
    }
    continuations += tail.iter().filter(|&&byte| byte & 0xc0 == 0x80).count() as u32;
    bytes.len() as u32 - continuations
}
