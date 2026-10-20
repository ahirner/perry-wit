//! ECMAScript whitespace recognition and trimming over UTF-8 bytes.

pub(crate) fn prefix(bytes: &[u8]) -> usize {
    match bytes {
        [b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c, ..] => 1,
        [0xc2, 0xa0, ..] => 2,
        [0xe1, 0x9a, 0x80, ..]
        | [0xe2, 0x80, 0x80..=0x8a | 0xa8 | 0xa9 | 0xaf, ..]
        | [0xe2, 0x81, 0x9f, ..]
        | [0xe3, 0x80, 0x80, ..]
        | [0xef, 0xbb, 0xbf, ..] => 3,
        _ => 0,
    }
}

pub fn trim_start_bytes(mut bytes: &[u8]) -> &[u8] {
    loop {
        let size = prefix(bytes);
        if size == 0 {
            return bytes;
        }
        bytes = &bytes[size..];
    }
}

pub fn trim_end_bytes(mut bytes: &[u8]) -> &[u8] {
    while let Some(start) = bytes.iter().rposition(|byte| byte & 0xc0 != 0x80) {
        let size = prefix(&bytes[start..]);
        if size == 0 || start + size != bytes.len() {
            break;
        }
        bytes = &bytes[..start];
    }
    bytes
}

pub fn trim_bytes(bytes: &[u8]) -> &[u8] {
    trim_end_bytes(trim_start_bytes(bytes))
}
