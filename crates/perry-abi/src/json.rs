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
    pub const INT64: u32 = 9;
    pub const UINT64: u32 = 10;
}
pub const MAX_DEPTH: u32 = 128;
pub use crate::layout::json::NEXT;
pub const TAIL: usize = 8;
pub const FIRST: usize = 16;
pub const MEMBER_COUNT: usize = 20;
pub const KEY: usize = 24;
pub const METADATA: usize = 28;
pub const NODE_BYTES: u32 = 32;
pub use crate::layout::record::PACKED_FIELDS;
const ONES: u64 = 0x0101_0101_0101_0101;
const HIGHS: u64 = 0x8080_8080_8080_8080;

/// Small records keep a sequence of one-based field indices in their header word.
pub fn append_field(mut order: u32, field: u32) -> u32 {
    assert!(field < PACKED_FIELDS as u32, "packed record field");
    let field = field + 1;
    for shift in (0..32).step_by(4) {
        let previous = (order >> shift) & 15;
        if previous == field {
            return order;
        }
        if previous == 0 {
            order |= field << shift;
            return order;
        }
    }
    panic!("packed record order is full")
}

/// Deletion removes the field's position, so a later insertion can append it.
pub fn remove_field(mut order: u32, field: u32) -> u32 {
    let mut kept = 0;
    let mut shift = 0;
    while order != 0 {
        let next = order & 15;
        if next != field + 1 {
            kept |= next << shift;
            shift += 4;
        }
        order >>= 4;
    }
    kept
}

/// Look up a field in the compiler's table sorted by UTF-8 length, then bytes.
///
/// # Safety
/// `table` contains `count` pairs of live string addresses and field indices.
pub unsafe fn field_index(
    table: *const u32,
    count: u32,
    compare: impl Fn(&[u8]) -> core::cmp::Ordering,
) -> u32 {
    let names = unsafe { core::slice::from_raw_parts(table, count as usize * 2) }
        .as_chunks::<2>()
        .0;
    let compare =
        |entry: &[u32; 2]| compare(unsafe { crate::bytes(entry[0] as usize as *mut u8) }).reverse();
    let index = if count < 16 {
        names.iter().position(|entry| compare(entry).is_eq())
    } else {
        names.binary_search_by(compare).ok()
    };
    index.map_or(count, |i| names[i][1])
}

/// Resolve a storage index in the compiler's indexed name table.
///
/// # Safety
/// `table` contains `count` pairs with each field index exactly once.
pub unsafe fn field_name(table: *const u32, count: u32, index: u32) -> *mut u8 {
    let names = unsafe { core::slice::from_raw_parts(table, count as usize * 2) }
        .as_chunks::<2>()
        .0;
    names
        .iter()
        .find(|entry| entry[1] == index)
        .expect("record field in schema")[0] as usize as *mut u8
}

/// The array index a key names (a canonical decimal below 2³² − 1), if any.
pub fn array_index(key: &[u8]) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key[0] == b'0' && key.len() > 1) {
        return None;
    }
    let mut value = 0u64;
    for &digit in key {
        if !digit.is_ascii_digit() {
            return None;
        }
        value = value * 10 + u64::from(digit - b'0');
    }
    (value < u64::from(u32::MAX)).then_some(value as u32)
}

/// Append an unlinked child to a JSON container under construction.
///
/// # Safety
/// Both pointers and the container's existing tail must address live, aligned `NODE_BYTES`
/// blocks in Wasm32 memory. The caller owns the container and child's link fields.
pub unsafe fn link_child(container: *mut u8, child: *mut u8) {
    unsafe {
        let tail = container.add(TAIL).cast::<u32>();
        let last = tail.read();
        let link = if last == 0 {
            container.add(FIRST)
        } else {
            (last as usize as *mut u8).add(NEXT)
        };
        link.cast::<u32>().write(child as usize as u32);
        tail.write(child as usize as u32);
        let count = container.add(MEMBER_COUNT).cast::<u32>();
        count.write(count.read() + 1);
    }
}

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
