//! Byte scans shared by the runtime and allocating helpers.

/// Compact scalar counter for minimal runtime components.
pub fn scalars_length(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .map(|&byte| u32::from(byte & 0xc0 != 0x80))
        .sum()
}

/// Eight-byte scalar counter for bulk helper input and output.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalar_counts_cross_word_boundaries() {
        for prefix in 0..24 {
            for suffix in 0..24 {
                let text = format!("{}é漢🙂\0{}", "a".repeat(prefix), "z".repeat(suffix));
                assert_eq!(
                    scalars_length(text.as_bytes()) as usize,
                    text.chars().count()
                );
                assert_eq!(
                    scalars_length_wide(text.as_bytes()),
                    scalars_length(text.as_bytes())
                );
            }
        }
        assert_eq!(scalars_length(b""), 0);
        assert_eq!(scalars_length_wide(b""), 0);
    }
}
