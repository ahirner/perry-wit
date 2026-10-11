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
const NUMBER_BYTES: usize = 5;
const ENTRY_BYTES: usize = 2 * NUMBER_BYTES;
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// Bytes needed for a UTF-8-safe static key table, including its names.
pub fn key_table_len<'a>(keys: impl ExactSizeIterator<Item = &'a str>) -> usize {
    NUMBER_BYTES + ENTRY_BYTES * keys.len() + keys.map(str::len).sum::<usize>()
}

/// Emit a static key table: five base-128 digits per count, length and byte offset.
pub fn encode_keys<'a>(
    keys: impl ExactSizeIterator<Item = &'a str> + Clone,
    mut emit: impl FnMut(&str),
) {
    let mut offset = NUMBER_BYTES + ENTRY_BYTES * keys.len();
    let mut number = |value: usize| {
        let digits: [u8; NUMBER_BYTES] =
            core::array::from_fn(|index| ((value >> (7 * index)) & 0x7f) as u8);
        emit(core::str::from_utf8(&digits).expect("base-128 digits are ASCII"));
    };
    number(keys.len());
    for name in keys.clone() {
        number(name.len());
        number(offset);
        offset += name.len();
    }
    for name in keys {
        emit(name);
    }
}

/// The index of `key` in a key table, or -1.
pub fn lookup(table: &[u8], hint: u32, key: &[u8]) -> i32 {
    let count = table_number(table, 0);
    // Compare lengths first: they are in the header, the key bytes are not.
    let matches = |index: u32| {
        table_number(table, NUMBER_BYTES + ENTRY_BYTES * index as usize) as usize == key.len()
            && table_key(table, index) == key
    };
    if hint < count && matches(hint) {
        return hint as i32;
    }
    (0..count)
        .find(|&index| index != hint && matches(index))
        .map_or(-1, |index| index as i32)
}

/// The five base-128 digits at `at` (least significant first).
///
/// Key tables are static strings the compiler generates (`perry-lower`'s `encode_keys`); a
/// short one is a compiler bug, so it traps instead of reading as some other table.
fn table_number(table: &[u8], at: usize) -> u32 {
    let &[a, b, c, d, e] = table
        .get(at..)
        .and_then(<[u8]>::first_chunk::<NUMBER_BYTES>)
        .expect("generated key tables hold every header number");
    u32::from(a)
        | (u32::from(b) << 7)
        | (u32::from(c) << 14)
        | (u32::from(d) << 21)
        | (u32::from(e) << 28)
}

/// The key bytes of entry `index`, which must be below the table's count.
pub fn table_key(table: &[u8], index: u32) -> &[u8] {
    let at = NUMBER_BYTES + ENTRY_BYTES * index as usize;
    let len = table_number(table, at) as usize;
    let offset = table_number(table, at + NUMBER_BYTES) as usize;
    table
        .get(offset..offset + len)
        .expect("generated key table entries lie within the table")
}

/// The length of the leading run of `bytes` that needs no escape, eight bytes at a time.
fn unescaped_prefix(bytes: &[u8]) -> usize {
    let mut index = 0;
    while let Some(word) = word(bytes, index)
        && special_bytes(word) == 0
    {
        index += 8;
    }
    while let Some(&byte) = bytes.get(index)
        && byte >= 0x20
        && byte != b'"'
        && byte != b'\\'
    {
        index += 1;
    }
    index
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

/// The eight little-endian bytes of `input` at `at`, if that many remain.
#[inline(always)]
pub fn word(input: &[u8], at: usize) -> Option<u64> {
    let bytes = input.get(at..)?.first_chunk::<8>()?;
    Some(u64::from_le_bytes(*bytes))
}

/// High bit set in the lowest quote, backslash or control byte of `word`: the bytes a JSON
/// string must escape (bits above the lowest may be spurious).
#[inline(always)]
pub fn special_bytes(word: u64) -> u64 {
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
