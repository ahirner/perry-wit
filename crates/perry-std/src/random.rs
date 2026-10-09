//! `Math.random`, `crypto.getRandomValues` and `crypto.randomUUID` over `wasi:random`.
use crate::runtime::{from_bytes, insecure_random_u64, random_u64};

/// `Math.random()`: 53 random bits from the insecure generator, in `[0, 1)`.
#[unsafe(no_mangle)]
pub extern "C" fn std_math_random() -> f64 {
    let bits = insecure_random_u64() >> 11;
    bits as f64 * (1.0 / (1u64 << 53) as f64)
}

/// `crypto.getRandomValues(bytes)`: fills the view from the secure generator; returns it.
/// # Safety
/// `bytes` must be a `Uint8Array`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_bytes_fill_random(bytes: *mut u8) -> *mut u8 {
    // SAFETY: Uint8Array layout of crate::bytes.
    let (len, data) = unsafe { (*(bytes as *const u32), *(bytes.add(4) as *const *mut u8)) };
    let mut index = 0;
    while index < len as usize {
        let word = random_u64().to_le_bytes();
        for byte in word.iter().take(len as usize - index) {
            // SAFETY: `index < len`.
            unsafe { *data.add(index) = *byte };
            index += 1;
        }
    }
    bytes
}

/// `crypto.randomUUID()`: a version 4 UUID in lowercase hex.
#[unsafe(no_mangle)]
pub extern "C" fn std_random_uuid() -> *mut u8 {
    let (high, low) = (random_u64(), random_u64());
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&high.to_be_bytes());
    bytes[8..].copy_from_slice(&low.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = [0u8; 36];
    let mut at = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            text[at] = b'-';
            at += 1;
        }
        text[at] = HEX[usize::from(byte >> 4)];
        text[at + 1] = HEX[usize::from(byte & 15)];
        at += 2;
    }
    from_bytes(&text)
}
