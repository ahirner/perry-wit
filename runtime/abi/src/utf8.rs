//! Byte scans shared by the instance runtime and standard libraries.

/// Compact scalar counter for minimal runtime components and constant text.
pub const fn scalars_length(bytes: &[u8]) -> u32 {
    let mut scalars = 0;
    let mut rest = bytes;
    while let [byte, tail @ ..] = rest {
        scalars += (*byte & 0xc0 != 0x80) as u32;
        rest = tail;
    }
    scalars
}

/// Static UTF-8 text with its scalar count, built by [static_text!](crate::static_text).
#[derive(Clone, Copy)]
pub struct StaticText {
    pub text: &'static str,
    pub scalars: u32,
}

impl StaticText {
    pub const fn new(text: &'static str) -> Self {
        Self {
            text,
            scalars: scalars_length(text.as_bytes()),
        }
    }
}

/// [StaticText] for a constant string, counted when the runtime is compiled.
#[macro_export]
macro_rules! static_text {
    ($text:expr) => {
        const { $crate::utf8::StaticText::new($text) }
    };
}

/// Eight-byte scalar counter for bulk UTF-8 input and output.
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
