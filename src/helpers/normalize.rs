//! Allocation-free canonical normalization with a bounded reorder buffer and partial NFK mappings.
#![cfg_attr(target_arch = "wasm32", no_std)]

use icu_properties::{CodePointMapData, props::CanonicalCombiningClass};

struct CanonicalPairRange {
    start: char,
    end: char,
    first: char,
    second_start: char,
}

include!(concat!(env!("OUT_DIR"), "/canonical.rs"));

fn canonical_parts(ch: char) -> Option<(char, Option<char>, Option<char>)> {
    if ch.is_ascii() {
        return None;
    }
    if let Some(pair) = PAIRS.get(PAIRS.partition_point(|entry| entry.end < ch))
        && pair.start <= ch
    {
        let second = char::from_u32(pair.second_start as u32 + ch as u32 - pair.start as u32)?;
        return Some((pair.first, Some(second), None));
    }
    let index = SINGLETONS
        .binary_search_by_key(&ch, |&(source, _)| source)
        .ok()?;
    Some((SINGLETONS[index].1, None, None))
}

fn compose_pair(first: char, second: char) -> Option<char> {
    let index = COMPOSITIONS.partition_point(|&index| {
        let pair = &PAIRS[usize::from(index)];
        (
            pair.first,
            pair.second_start as u32 + pair.end as u32 - pair.start as u32,
        ) < (first, second as u32)
    });
    let pair = &PAIRS[usize::from(*COMPOSITIONS.get(index)?)];
    let offset = (second as u32).checked_sub(pair.second_start as u32)?;
    (pair.first == first && offset <= pair.end as u32 - pair.start as u32)
        .then(|| char::from_u32(pair.start as u32 + offset))
        .flatten()
}

/// Normalizes UTF-8 text (`mode` bit 0 = compose, bit 1 = compat), returning packed
/// byte/scalar lengths, or u64::MAX on overflow. With `out_ptr = 0`, measures without writing.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn str_normalize(
    src_ptr: u32,
    src_byte_len: u32,
    out_ptr: u32,
    mode: u32,
) -> u64 {
    if src_byte_len == 0 {
        return 0;
    }
    // SAFETY: compiled callers supply valid UTF-8 and a separate output allocation.
    let input = unsafe {
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            src_ptr as *const u8,
            src_byte_len as usize,
        ))
    };
    normalize_utf8(input, out_ptr, (mode & 1) != 0, (mode & 2) != 0)
}

fn normalize_utf8(input: &str, out_ptr: u32, compose: bool, compat: bool) -> u64 {
    let mut sink = OutSink {
        out_ptr,
        byte_len: 0,
        scalar_len: 0,
        overflow: false,
    };
    let mut run = CharRun {
        buf: [('\0', 0); 64],
        len: 0,
    };
    for ch in input.chars() {
        let mut stack = [ch; 8];
        let mut pending = 1;
        while pending != 0 {
            pending -= 1;
            let curr = stack[pending];
            let parts =
                canonical_parts(curr).or_else(|| compat.then(|| compat_triple(curr)).flatten());
            if let Some((a, b, c)) = parts {
                for part in [c, b, Some(a)].into_iter().flatten() {
                    stack[pending] = part;
                    pending += 1;
                }
            } else {
                run.push(curr, &mut sink, compose);
            }
        }
        if sink.overflow {
            return u64::MAX;
        }
    }
    if compose {
        run.compose();
    }
    run.emit(&mut sink);
    if sink.overflow {
        u64::MAX
    } else {
        u64::from(sink.byte_len) | (u64::from(sink.scalar_len) << 32)
    }
}

struct OutSink {
    out_ptr: u32,
    byte_len: u32,
    scalar_len: u32,
    overflow: bool,
}

impl OutSink {
    fn emit(&mut self, ch: char) {
        if self.overflow {
            return;
        }
        let width = ch.len_utf8() as u32;
        let Some(next_len) = self.byte_len.checked_add(width) else {
            self.overflow = true;
            return;
        };
        if self.out_ptr != 0 {
            let Some(dest) = self.out_ptr.checked_add(self.byte_len) else {
                self.overflow = true;
                return;
            };
            // SAFETY: measurement reserved the complete output, including this scalar.
            let output =
                unsafe { core::slice::from_raw_parts_mut(dest as *mut u8, width as usize) };
            ch.encode_utf8(output);
        }
        self.byte_len = next_len;
        self.scalar_len += 1;
    }
}

struct CharRun {
    buf: [(char, u8); 64],
    len: usize,
}

impl CharRun {
    fn push(&mut self, ch: char, out: &mut OutSink, compose: bool) {
        let ccc = CodePointMapData::<CanonicalCombiningClass>::new().get(ch).0;
        if ccc == 0 || self.len == self.buf.len() {
            if compose {
                self.compose();
                if ccc == 0
                    && self.len == 1
                    && self.buf[0].1 == 0
                    && let Some(composed) = compose_pair(self.buf[0].0, ch)
                {
                    self.buf[0].0 = composed;
                    return;
                }
            }
            self.emit(out);
        }
        let mut at = self.len;
        while at > 0 && ccc != 0 && self.buf[at - 1].1 > ccc {
            self.buf[at] = self.buf[at - 1];
            at -= 1;
        }
        self.buf[at] = (ch, ccc);
        self.len += 1;
    }

    fn compose(&mut self) {
        if self.len < 2 || self.buf[0].1 != 0 {
            return;
        }
        let mut last_ccc = 0;
        let mut write = 1;
        for read in 1..self.len {
            let (ch, ccc) = self.buf[read];
            if (last_ccc < ccc || last_ccc == 0)
                && let Some(composed) = compose_pair(self.buf[0].0, ch)
            {
                self.buf[0].0 = composed;
                continue;
            }
            last_ccc = ccc;
            self.buf[write] = (ch, ccc);
            write += 1;
        }
        self.len = write;
    }

    fn emit(&mut self, out: &mut OutSink) {
        for &(ch, _) in &self.buf[..self.len] {
            out.emit(ch);
        }
        self.len = 0;
    }
}

fn compat_triple(ch: char) -> Option<(char, Option<char>, Option<char>)> {
    Some(match ch {
        '\u{FF01}'..='\u{FF5E}' => (char::from_u32(ch as u32 - 0xFEE0)?, None, None),
        '\u{00A0}' => (' ', None, None),
        '\u{00A8}' => (' ', Some('\u{0308}'), None),
        '\u{00AA}' => ('a', None, None),
        '\u{00AF}' => (' ', Some('\u{0304}'), None),
        '\u{00B2}' => ('2', None, None),
        '\u{00B3}' => ('3', None, None),
        '\u{00B4}' => (' ', Some('\u{0301}'), None),
        '\u{00B5}' => ('\u{03BC}', None, None),
        '\u{00B8}' => (' ', Some('\u{0327}'), None),
        '\u{00B9}' => ('1', None, None),
        '\u{00BA}' => ('o', None, None),
        '\u{00BC}' => ('1', Some('\u{2044}'), Some('4')),
        '\u{00BD}' => ('1', Some('\u{2044}'), Some('2')),
        '\u{00BE}' => ('3', Some('\u{2044}'), Some('4')),
        '\u{0132}' => ('I', Some('J'), None),
        '\u{0133}' => ('i', Some('j'), None),
        '\u{013F}' => ('L', Some('\u{00B7}'), None),
        '\u{0140}' => ('l', Some('\u{00B7}'), None),
        '\u{0149}' => ('\u{02BC}', Some('n'), None),
        '\u{017F}' => ('s', None, None),
        '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => (' ', None, None),
        '\u{FB00}' => ('f', Some('f'), None),
        '\u{FB01}' => ('f', Some('i'), None),
        '\u{FB02}' => ('f', Some('l'), None),
        '\u{FB03}' => ('f', Some('f'), Some('i')),
        '\u{FB04}' => ('f', Some('f'), Some('l')),
        _ => return None,
    })
}

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
