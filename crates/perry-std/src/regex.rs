//! Statically checked reverse and anchored-forward DFA execution with optional NFA captures.
use crate::runtime;
use perry_abi::layout::regex::{
    FORWARD_CLASSES, FORWARD_ROWS, GLOBAL, LookCode, MAX_CAPTURES, MAX_PENDING, MAX_STATES,
    MAX_THREADS, NfaState, NfaTable, REVERSE_CLASSES, REVERSE_ROWS,
};

#[inline(always)]
unsafe fn read_u16(ptr: *const u8) -> u16 {
    unsafe { ptr.cast::<u16>().read_unaligned() }
}

#[inline(always)]
unsafe fn read_u32(ptr: *const u8) -> u32 {
    unsafe { ptr.cast::<u32>().read_unaligned() }
}

#[inline(always)]
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn look_matches(code: LookCode, text: &[u8], cursor: usize) -> bool {
    let prev_word = cursor > 0 && text.get(cursor - 1).is_some_and(|&b| is_word_byte(b));
    let curr_word = text.get(cursor).is_some_and(|&b| is_word_byte(b));
    match code {
        LookCode::Start => cursor == 0,
        LookCode::End => cursor == text.len(),
        LookCode::StartLine => cursor == 0 || text.get(cursor - 1) == Some(&b'\n'),
        LookCode::EndLine => cursor == text.len() || text.get(cursor) == Some(&b'\n'),
        LookCode::WordBoundary => prev_word != curr_word,
        LookCode::NotWordBoundary => prev_word == curr_word,
        LookCode::WordStart => !prev_word && curr_word,
        LookCode::WordEnd => prev_word && !curr_word,
    }
}

type Slots = [u32; 2 * MAX_CAPTURES];
const MAX_SLOTS: usize = 2 * MAX_CAPTURES;

/// # Safety
/// `program` must be a compiler-generated regex table.
unsafe fn group_count(program: *const u8) -> usize {
    let count = unsafe { read_u16(program.add(2)) } as usize;
    assert!((1..=1 + MAX_CAPTURES).contains(&count), "regex group count");
    count
}

struct Threads<'a> {
    entries: [(&'a [[u8; 4]], Slots); MAX_THREADS],
    len: usize,
}

impl Default for Threads<'_> {
    fn default() -> Self {
        Self {
            entries: [(&[], [0; MAX_SLOTS]); MAX_THREADS],
            len: 0,
        }
    }
}

struct Nfa<'a> {
    table: NfaTable<'a>,
    text: &'a [u8],
    end: usize,
    slots: Slots,
    matched: Slots,
    visited: [u64; MAX_STATES / 64],
}

impl<'a> Nfa<'a> {
    fn closure(&mut self, cursor: usize, seed: u16, next: &mut Threads<'a>) -> bool {
        let mut stack = [(0u32, 0u32); MAX_PENDING];
        let mut len = 1;
        stack[0] = (u32::from(seed), 0);
        while len > 0 {
            len -= 1;
            let (tag, previous) = stack[len];
            if tag & 0x8000_0000 != 0 {
                self.slots[(tag & 0x7fff_ffff) as usize] = previous;
                continue;
            }
            let state = tag as u16;
            let word = (state >> 6) as usize;
            let bit = 1u64 << (state & 63);
            if self.visited[word] & bit != 0 {
                continue;
            }
            self.visited[word] |= bit;
            match self.table.state(state) {
                NfaState::Ranges(ranges) => {
                    next.entries[next.len] = (ranges, self.slots);
                    next.len += 1;
                }
                NfaState::Look(code, target) => {
                    if look_matches(code, self.text, cursor) {
                        stack[len] = (u32::from(target), 0);
                        len += 1;
                    }
                }
                NfaState::Union(alternates) => {
                    for &target in alternates.iter().rev() {
                        stack[len] = (u32::from(u16::from_le_bytes(target)), 0);
                        len += 1;
                    }
                }
                NfaState::Capture(slot, target) => {
                    // The DFA supplies whole-match bounds; only explicit groups need slots.
                    if let Some(slot) = slot.checked_sub(2) {
                        let previous = self.slots[usize::from(slot)];
                        self.slots[usize::from(slot)] = cursor as u32 + 1;
                        stack[len] = (u32::from(slot) | 0x8000_0000, previous);
                        len += 1;
                    }
                    stack[len] = (u32::from(target), 0);
                    len += 1;
                }
                NfaState::Match if cursor == self.end => {
                    self.matched = self.slots;
                    return true;
                }
                NfaState::Fail | NfaState::Match => {}
            }
        }
        false
    }
}

unsafe fn extract_captures(
    program: *const u8,
    text: &[u8],
    start: usize,
    end: usize,
    matched_slots: &mut Slots,
) -> usize {
    *matched_slots = [0; MAX_SLOTS];
    let group_count = unsafe { group_count(program) };
    if group_count <= 1 {
        return 0;
    }
    let fwd_header = unsafe { read_u32(program.add(4)) };
    let fwd_base = unsafe { program.add((fwd_header & !GLOBAL) as usize) };
    let nfa_offset = unsafe { read_u32(fwd_base.add(8)) } as usize;
    assert!(nfa_offset != 0, "missing regex capture program");
    let bytes = unsafe {
        let base = program.add(nfa_offset);
        core::slice::from_raw_parts(base, read_u32(base) as usize)
    };
    let mut nfa = Nfa {
        table: NfaTable::read(bytes),
        text,
        end,
        slots: [0; MAX_SLOTS],
        matched: [0; MAX_SLOTS],
        visited: [0; MAX_STATES / 64],
    };
    let mut current = Threads::default();
    let mut next = Threads::default();
    nfa.closure(start, nfa.table.start(), &mut current);
    for (cursor, &byte) in text.iter().enumerate().take(end).skip(start) {
        if current.len == 0 {
            break;
        }
        next.len = 0;
        nfa.visited.fill(0);
        for &(ranges, slots) in &current.entries[..current.len] {
            let target = ranges.iter().find(|r| r[0] <= byte && byte <= r[1]);
            if let Some(&[_, _, lo, hi]) = target {
                nfa.slots = slots;
                if nfa.closure(cursor + 1, u16::from_le_bytes([lo, hi]), &mut next) {
                    break;
                }
            }
        }
        core::mem::swap(&mut current, &mut next);
    }
    *matched_slots = nfa.matched;
    group_count - 1
}

/// Find the leftmost match start at or after byte offset `pos`.
///
/// # Safety
/// `program` must be a compiler-generated reverse+forward DFA table.
unsafe fn match_start(text: &[u8], pos: usize, program: *const u8) -> Option<usize> {
    if pos > text.len() {
        return None;
    }
    let rev_end_column = unsafe { read_u16(program) } as usize;
    let rev_rows = unsafe { program.add(REVERSE_ROWS) };
    let mut state = rev_rows;
    let mut best_start = None;
    for cursor in (pos..text.len()).rev() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *program.add(REVERSE_CLASSES + byte as usize) };
        let offset = unsafe { read_u16(state.add(2 + usize::from(class) * 2)) };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { read_u16(state) } != 0 && text.get(cursor + 1).is_none_or(|&b| b & 0xc0 != 0x80)
        {
            best_start = Some(cursor + 1);
        }
    }
    if pos == 0 {
        let offset = unsafe { read_u16(state.add(rev_end_column)) };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { read_u16(state) } != 0 {
            best_start = Some(0);
        }
    } else if let Some(&prev) = text.get(pos - 1) {
        let class = unsafe { *program.add(REVERSE_CLASSES + prev as usize) };
        let offset = unsafe { read_u16(state.add(2 + usize::from(class) * 2)) };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { read_u16(state) } != 0 && text.get(pos).is_none_or(|&b| b & 0xc0 != 0x80) {
            best_start = Some(pos);
        }
    }
    best_start
}

unsafe fn next_match(text: &[u8], pos: usize, program: *const u8) -> Option<(usize, usize)> {
    let start = unsafe { match_start(text, pos, program) }?;

    let fwd_header = unsafe { read_u32(program.add(4)) };
    let fwd_base = unsafe { program.add((fwd_header & !GLOBAL) as usize) };
    let fwd_end_column = unsafe { read_u16(fwd_base) } as usize;
    let start_offset = if start == 0 {
        unsafe { read_u16(fwd_base.add(2)) }
    } else if text.get(start - 1).is_some_and(|&b| is_word_byte(b)) {
        unsafe { read_u16(fwd_base.add(4)) }
    } else {
        unsafe { read_u16(fwd_base.add(6)) }
    };
    let fwd_rows = unsafe { fwd_base.add(FORWARD_ROWS) };
    let mut state = unsafe { fwd_rows.add(start_offset as usize) };
    let mut best_end = None;
    let mut hit_dead = false;
    for cursor in start..text.len() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *fwd_base.add(FORWARD_CLASSES + byte as usize) };
        let offset = unsafe { read_u16(state.add(2 + usize::from(class) * 2)) };
        state = unsafe { fwd_rows.add(offset as usize) };
        let flags = unsafe { read_u16(state) };
        if flags & 1 != 0 && (cursor == start || byte & 0xc0 != 0x80) {
            best_end = Some(cursor);
        }
        if flags & 2 != 0 {
            hit_dead = true;
            break;
        }
    }
    if !hit_dead {
        let offset = unsafe { read_u16(state.add(fwd_end_column)) };
        state = unsafe { fwd_rows.add(offset as usize) };
        if unsafe { read_u16(state) } & 1 != 0 {
            best_end = Some(text.len());
        }
    }
    Some((start, best_end?))
}

fn next_scalar_boundary(text: &[u8], pos: usize) -> usize {
    let mut next = pos + 1;
    while let Some(&b) = text.get(next) {
        if b & 0xc0 != 0x80 {
            break;
        }
        next += 1;
    }
    next
}

#[inline]
unsafe fn slice_string(string: *mut u8, len: usize, start: usize, end: usize) -> *mut u8 {
    if start == 0 && end == len {
        string
    } else if start == end {
        runtime::from_static("")
    } else {
        unsafe { runtime::string_view(string, start as u32, end as u32) }
    }
}

pub(crate) fn push_substitution(
    out: &mut runtime::Buffer,
    text: &[u8],
    start: usize,
    end: usize,
    replacement: &[u8],
    captures: &[u32],
) {
    if !replacement.contains(&b'$') {
        out.extend_from_slice(replacement);
        return;
    }
    let group_count = captures.len() / 2;
    let mut index = 0;
    while let Some(&ch) = replacement.get(index) {
        if ch == b'$'
            && let Some(&next) = replacement.get(index + 1)
        {
            match next {
                b'$' => {
                    out.push(b'$');
                    index += 2;
                }
                b'&' => {
                    out.extend_from_slice(&text[start..end]);
                    index += 2;
                }
                b'`' => {
                    out.extend_from_slice(&text[..start]);
                    index += 2;
                }
                b'\'' => {
                    out.extend_from_slice(&text[end..]);
                    index += 2;
                }
                b'0'..=b'9' if group_count > 0 => {
                    let d1 = (next - b'0') as usize;
                    if let Some(&d2_ch @ b'0'..=b'9') = replacement.get(index + 2) {
                        let two = d1 * 10 + (d2_ch - b'0') as usize;
                        if two >= 1 && two <= group_count {
                            let s = captures[(two - 1) * 2];
                            let e = captures[(two - 1) * 2 + 1];
                            if s != 0 && e != 0 && s <= e {
                                out.extend_from_slice(&text[(s as usize - 1)..(e as usize - 1)]);
                            }
                            index += 3;
                            continue;
                        }
                    }
                    if d1 >= 1 && d1 <= group_count {
                        let s = captures[(d1 - 1) * 2];
                        let e = captures[(d1 - 1) * 2 + 1];
                        if s != 0 && e != 0 && s <= e {
                            out.extend_from_slice(&text[(s as usize - 1)..(e as usize - 1)]);
                        }
                        index += 2;
                    } else {
                        out.push(b'$');
                        index += 1;
                    }
                }
                _ => {
                    out.push(b'$');
                    index += 1;
                }
            }
        } else {
            out.push(ch);
            index += 1;
        }
    }
}

/// # Safety
/// `text` is a live immutable string; `program` is a compiler-generated DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_search(text: *mut u8, program: *const u8) -> f64 {
    let text = unsafe { runtime::bytes(text) };
    unsafe { match_start(text, 0, program) }.map_or(-1.0, |start| {
        text[..start]
            .iter()
            .filter(|&&byte| byte & 0xc0 != 0x80)
            .count() as f64
    })
}

/// `s.replace(regex, replacement)` and `s.replaceAll(regex, replacement)`.
///
/// # Safety
/// `string` and `replacement` are live immutable strings; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_replace(
    string: *mut u8,
    program: *const u8,
    replacement: *mut u8,
) -> *mut u8 {
    let text = unsafe { runtime::bytes(string) };
    let Some((first_start, first_end)) = (unsafe { next_match(text, 0, program) }) else {
        return string;
    };
    let rep = unsafe { runtime::bytes(replacement) };
    let needs_captures = rep.contains(&b'$') && unsafe { group_count(program) } > 1;
    let mut cap_slots = [0u32; MAX_SLOTS];
    let is_global = (unsafe { read_u32(program.add(4)) } >> 31) != 0;
    let mut out = runtime::Buffer::new();
    out.extend_from_slice(&text[..first_start]);
    let cap_len = if needs_captures {
        unsafe { extract_captures(program, text, first_start, first_end, &mut cap_slots) * 2 }
    } else {
        0
    };
    push_substitution(
        &mut out,
        text,
        first_start,
        first_end,
        rep,
        &cap_slots[..cap_len],
    );
    if !is_global {
        out.extend_from_slice(&text[first_end..]);
        return unsafe { out.into_string() };
    }
    let mut last_end = first_end;
    let mut pos = if first_end > first_start {
        first_end
    } else if first_end < text.len() {
        next_scalar_boundary(text, first_end)
    } else {
        out.extend_from_slice(&text[last_end..]);
        return unsafe { out.into_string() };
    };
    while let Some((start, end)) = unsafe { next_match(text, pos, program) } {
        out.extend_from_slice(&text[last_end..start]);
        let cap_len = if needs_captures {
            unsafe { extract_captures(program, text, start, end, &mut cap_slots) * 2 }
        } else {
            0
        };
        push_substitution(&mut out, text, start, end, rep, &cap_slots[..cap_len]);
        last_end = end;
        if end > start {
            pos = end;
        } else if end < text.len() {
            pos = next_scalar_boundary(text, end);
        } else {
            break;
        }
    }
    out.extend_from_slice(&text[last_end..]);
    unsafe { out.into_string() }
}

/// `regex.exec(s)`.
///
/// # Safety
/// `string` is a live immutable string; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_exec(string: *mut u8, program: *const u8) -> *mut u8 {
    let text = unsafe { runtime::bytes(string) };
    let Some((start, end)) = (unsafe { next_match(text, 0, program) }) else {
        return core::ptr::null_mut();
    };
    let group_count = unsafe { group_count(program) };
    let mut cap_slots = [0u32; MAX_SLOTS];
    if group_count > 1 {
        unsafe { extract_captures(program, text, start, end, &mut cap_slots) };
    }
    let (array, slots) = runtime::array_storage(group_count);
    if let Some(first) = slots.first_mut() {
        *first = unsafe { slice_string(string, text.len(), start, end) } as u64;
    }
    for (i, slot) in slots.iter_mut().enumerate().skip(1) {
        let s = cap_slots[(i - 1) * 2];
        let e = cap_slots[(i - 1) * 2 + 1];
        let piece = if s != 0 && e != 0 && s <= e {
            unsafe { slice_string(string, text.len(), s as usize - 1, e as usize - 1) }
        } else {
            runtime::from_static("")
        };
        *slot = piece as u64;
    }
    array
}

/// `s.match(regex)`.
///
/// # Safety
/// `string` is a live immutable string; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_match(string: *mut u8, program: *const u8) -> *mut u8 {
    let is_global = (unsafe { read_u32(program.add(4)) } >> 31) != 0;
    if !is_global {
        return unsafe { std_regex_exec(string, program) };
    }
    let text = unsafe { runtime::bytes(string) };
    let matches = || {
        let mut pos = 0usize;
        let mut finished = false;
        core::iter::from_fn(move || {
            if finished {
                return None;
            }
            let (start, end) = unsafe { next_match(text, pos, program) }?;
            if end > start {
                pos = end;
            } else if end < text.len() {
                pos = next_scalar_boundary(text, end);
            } else {
                finished = true;
            }
            Some((start, end))
        })
    };
    let count = matches().count();
    if count == 0 {
        return core::ptr::null_mut();
    }
    let (array, slots) = runtime::array_storage(count);
    for (slot, (start, end)) in slots.iter_mut().zip(matches()) {
        *slot = unsafe { slice_string(string, text.len(), start, end) } as u64;
    }
    array
}

/// `s.split(regex)`.
///
/// # Safety
/// `string` is a live immutable string; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_split(string: *mut u8, program: *const u8) -> *mut u8 {
    let text = unsafe { runtime::bytes(string) };
    if text.is_empty() {
        if (unsafe { next_match(text, 0, program) }).is_some() {
            let (array, _) = runtime::array_storage(0);
            return array;
        }
        let (array, slots) = runtime::array_storage(1);
        if let Some(slot) = slots.first_mut() {
            *slot = string as u64;
        }
        return array;
    }
    let chunks = || {
        let mut last_end = 0usize;
        let mut pos = 0usize;
        let mut finished = false;
        core::iter::from_fn(move || {
            if finished {
                return None;
            }
            while pos < text.len() {
                let Some((start, end)) = (unsafe { next_match(text, pos, program) }) else {
                    break;
                };
                if start == text.len() && end == text.len() {
                    break;
                }
                if end == last_end {
                    pos = next_scalar_boundary(text, pos);
                } else {
                    let range = (last_end, start);
                    last_end = end;
                    pos = end;
                    return Some(range);
                }
            }
            finished = true;
            Some((last_end, text.len()))
        })
    };
    let (array, slots) = runtime::array_storage(chunks().count());
    for (slot, (start, end)) in slots.iter_mut().zip(chunks()) {
        *slot = unsafe { slice_string(string, text.len(), start, end) } as u64;
    }
    array
}
