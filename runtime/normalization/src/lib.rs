//! Canonical normalization with inline run storage and partial NFK mappings.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

use icu_properties::{CodePointMapData, props::CanonicalCombiningClass};
use perry_rt_sys::Buffer;

use guest_memory::OutSink;
use perry_rt_sys::guest_memory;

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
    let input = unsafe { guest_memory::str_from_raw(src_ptr, src_byte_len) };
    normalize_utf8(input, out_ptr, (mode & 1) != 0, (mode & 2) != 0)
}

fn normalize_utf8(input: &str, out_ptr: u32, compose: bool, compat: bool) -> u64 {
    let mut sink = OutSink::new(out_ptr);
    let mut inline = [[0; 8]; 64];
    let mut run = CharRun {
        storage: RunStorage::Inline(&mut inline),
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
    run.order(compose);
    run.emit(&mut sink);
    sink.finish()
}

struct CharRun<'a> {
    storage: RunStorage<'a>,
    len: usize,
}

enum RunStorage<'a> {
    Inline(&'a mut [[u8; 8]]),
    Spill(Buffer),
}

fn character(entry: &[u8; 8]) -> char {
    char::from_u32((u64::from_ne_bytes(*entry) & 0xff_ffff) as u32)
        .expect("run entries contain Unicode scalars")
}

fn set_character(entry: &mut [u8; 8], ch: char) {
    *entry = (u64::from_ne_bytes(*entry) & !0xff_ffff | u64::from(ch)).to_ne_bytes();
}

fn combining_class(entry: &[u8; 8]) -> u8 {
    (u64::from_ne_bytes(*entry) >> 56) as u8
}

impl CharRun<'_> {
    fn entries(&self) -> &[[u8; 8]] {
        match &self.storage {
            RunStorage::Inline(entries) => &entries[..self.len],
            RunStorage::Spill(buffer) => buffer.as_slice().as_chunks::<8>().0,
        }
    }

    fn entries_mut(&mut self) -> &mut [[u8; 8]] {
        match &mut self.storage {
            RunStorage::Inline(entries) => &mut entries[..self.len],
            RunStorage::Spill(buffer) => buffer.as_mut_slice().as_chunks_mut::<8>().0,
        }
    }

    fn push(&mut self, ch: char, out: &mut OutSink, compose: bool) {
        let ccc = CodePointMapData::<CanonicalCombiningClass>::new().get(ch).0;
        if ccc == 0 {
            self.order(compose);
            if compose
                && self.len == 1
                && combining_class(&self.entries()[0]) == 0
                && let Some(composed) = compose_pair(character(&self.entries()[0]), ch)
            {
                set_character(&mut self.entries_mut()[0], composed);
                return;
            }
            self.emit(out);
        }
        if let RunStorage::Inline(entries) = &self.storage
            && self.len == entries.len()
        {
            let mut spill = Buffer::new();
            spill.reserve(self.len * 16);
            spill.extend_from_slice(entries.as_flattened());
            self.storage = RunStorage::Spill(spill);
        }
        // Keys sort by class, then original position, preserving equal-class order.
        let entry = (u64::from(ccc) << 56 | (self.len as u64) << 24 | u64::from(ch)).to_ne_bytes();
        match &mut self.storage {
            RunStorage::Inline(entries) => entries[self.len] = entry,
            RunStorage::Spill(buffer) => buffer.extend_from_slice(&entry),
        }
        self.len += 1;
    }

    fn order(&mut self, compose: bool) {
        let entries = self.entries_mut();
        entries.sort_unstable_by_key(|entry| u64::from_ne_bytes(*entry));
        if !compose || entries.len() < 2 || combining_class(&entries[0]) != 0 {
            return;
        }
        let mut last_ccc = 0;
        let mut write = 1;
        for read in 1..entries.len() {
            let ccc = combining_class(&entries[read]);
            if (last_ccc < ccc || last_ccc == 0)
                && let Some(composed) =
                    compose_pair(character(&entries[0]), character(&entries[read]))
            {
                set_character(&mut entries[0], composed);
                continue;
            }
            last_ccc = ccc;
            entries[write] = entries[read];
            write += 1;
        }
        self.len = write;
        if let RunStorage::Spill(buffer) = &mut self.storage {
            buffer.clear();
            buffer.advance(write * 8);
        }
    }

    fn emit(&mut self, out: &mut OutSink) {
        for entry in &self.entries()[..self.len] {
            let _ = out.emit(character(entry));
        }
        if let RunStorage::Spill(buffer) = &mut self.storage {
            buffer.clear();
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
