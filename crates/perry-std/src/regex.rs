//! Statically checked reverse and anchored-forward DFA execution with optional NFA captures.
use crate::runtime;
use perry_abi::layout::array;
use perry_abi::layout::regex::{
    FORWARD_CLASSES, FORWARD_ROWS, GLOBAL, LookCode, MAX_CAPTURES, MAX_PENDING, MAX_STATES,
    MAX_THREADS, NAMES_OFFSET, NfaState, NfaTable, RESULT_BYTES, RESULT_GROUPS, RESULT_INDEX,
    RESULT_INPUT, RESULT_NEXT, REVERSE_CLASSES, REVERSE_ROWS,
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
    let nfa_offset = unsafe { read_u32(forward_base(program).add(8)) } as usize;
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

/// Start of the forward DFA, whose offset shares the header word with the `g` flag.
///
/// # Safety
/// `program` must be a compiler-generated regex table.
unsafe fn forward_base(program: *const u8) -> *const u8 {
    unsafe { program.add((read_u32(program.add(4)) & !GLOBAL) as usize) }
}

/// # Safety
/// `program` must be a compiler-generated regex table.
unsafe fn is_global(program: *const u8) -> bool {
    (unsafe { read_u32(program.add(4)) } & GLOBAL) != 0
}

/// One direction of a compiled table: rows are `[flags: u16, next row offset per class: u16...]`.
#[derive(Clone, Copy)]
struct Dfa {
    rows: *const u8,
    classes: *const u8,
    end_column: usize,
}

impl Dfa {
    /// # Safety
    /// `base` heads a compiler-generated DFA whose classes and rows sit at the given offsets.
    unsafe fn new(base: *const u8, classes: usize, rows: usize) -> Self {
        unsafe {
            Self {
                rows: base.add(rows),
                classes: base.add(classes),
                end_column: usize::from(read_u16(base)),
            }
        }
    }

    /// # Safety
    /// `state` is a row of this table.
    unsafe fn step(self, state: *const u8, byte: u8) -> *const u8 {
        let class = unsafe { *self.classes.add(usize::from(byte)) };
        unsafe { self.goto(state, 2 + usize::from(class) * 2) }
    }

    /// Transition on end of input.
    ///
    /// # Safety
    /// `state` is a row of this table.
    unsafe fn end(self, state: *const u8) -> *const u8 {
        unsafe { self.goto(state, self.end_column) }
    }

    /// # Safety
    /// `column` of `state` holds a row offset of this table.
    unsafe fn goto(self, state: *const u8, column: usize) -> *const u8 {
        unsafe { self.rows.add(usize::from(read_u16(state.add(column)))) }
    }

    /// # Safety
    /// `state` is a row of this table.
    unsafe fn flags(state: *const u8) -> u16 {
        unsafe { read_u16(state) }
    }
}

/// Whether `at` is a UTF-8 scalar boundary (or the end).
fn scalar_boundary(text: &[u8], at: usize) -> bool {
    text.get(at).is_none_or(|&b| b & 0xc0 != 0x80)
}

/// Find the leftmost match start at or after byte offset `pos`.
///
/// # Safety
/// `program` must be a compiler-generated reverse+forward DFA table.
unsafe fn match_start(text: &[u8], pos: usize, program: *const u8) -> Option<usize> {
    let tail = text.get(pos..)?;
    let dfa = unsafe { Dfa::new(program, REVERSE_CLASSES, REVERSE_ROWS) };
    let mut state = dfa.rows;
    let mut best_start = None;
    for (offset, &byte) in tail.iter().enumerate().rev() {
        state = unsafe { dfa.step(state, byte) };
        let cursor = pos + offset + 1;
        if unsafe { Dfa::flags(state) } != 0 && scalar_boundary(text, cursor) {
            best_start = Some(cursor);
        }
    }
    // The byte before `pos` (or the start of input) decides whether `pos` itself starts a match.
    state = match pos.checked_sub(1) {
        Some(prev) => unsafe { dfa.step(state, text[prev]) },
        None => unsafe { dfa.end(state) },
    };
    if unsafe { Dfa::flags(state) } != 0 && scalar_boundary(text, pos) {
        best_start = Some(pos);
    }
    best_start
}

unsafe fn next_match(text: &[u8], pos: usize, program: *const u8) -> Option<(usize, usize)> {
    let start = unsafe { match_start(text, pos, program) }?;
    let base = unsafe { forward_base(program) };
    let dfa = unsafe { Dfa::new(base, FORWARD_CLASSES, FORWARD_ROWS) };
    // The header's start-state columns (2: input start, 4: after a word byte, 6: otherwise)
    // hold row offsets like a row does.
    let start_column = match start.checked_sub(1) {
        None => 2,
        Some(prev) if is_word_byte(text[prev]) => 4,
        Some(_) => 6,
    };
    let mut state = unsafe { dfa.goto(base, start_column) };
    let mut best_end = None;
    for (cursor, &byte) in text.iter().enumerate().skip(start) {
        state = unsafe { dfa.step(state, byte) };
        let flags = unsafe { Dfa::flags(state) };
        if flags & 1 != 0 && (cursor == start || byte & 0xc0 != 0x80) {
            best_end = Some(cursor);
        }
        if flags & 2 != 0 {
            return Some((start, best_end?));
        }
    }
    if unsafe { Dfa::flags(dfa.end(state)) } & 1 != 0 {
        best_end = Some(text.len());
    }
    Some((start, best_end?))
}

/// Successive non-overlapping matches, stepping one scalar past empty matches like `lastIndex`.
///
/// # Safety
/// `program` must remain a valid compiler-generated DFA table while iterating.
unsafe fn matches(text: &[u8], program: *const u8) -> impl Iterator<Item = (usize, usize)> {
    let mut pos = Some(0);
    core::iter::from_fn(move || {
        let (start, end) = unsafe { next_match(text, pos?, program) }?;
        pos = if end > start {
            Some(end)
        } else if end < text.len() {
            Some(next_scalar_boundary(text, end))
        } else {
            None
        };
        Some((start, end))
    })
}

/// Byte range of capture `group` (1-based); slots store offset + 1 so 0 means unset.
fn capture(slots: &[u32], group: usize) -> Option<core::ops::Range<usize>> {
    let (s, e) = (slots[2 * group - 2], slots[2 * group - 1]);
    (s != 0 && e != 0 && s <= e).then(|| s as usize - 1..e as usize - 1)
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
    substitute(out, text, start, end, replacement, captures, None);
}

#[derive(Clone, Copy)]
struct Names(*const u8);

impl Iterator for Names {
    type Item = (usize, &'static [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        // Names only comes from an immutable compiler table, terminated by group zero.
        unsafe {
            let group = read_u16(self.0) as usize;
            if group == 0 {
                return None;
            }
            let len = read_u16(self.0.add(2)) as usize;
            let name = core::slice::from_raw_parts(self.0.add(4), len);
            self.0 = self.0.add(4 + len);
            Some((group, name))
        }
    }
}

unsafe fn names(program: *const u8) -> Option<Names> {
    let table = unsafe { program.add(read_u32(program.add(NAMES_OFFSET)) as usize) };
    (unsafe { read_u16(table) } != 0).then_some(Names(table))
}

fn substitute(
    out: &mut runtime::Buffer,
    text: &[u8],
    start: usize,
    end: usize,
    replacement: &[u8],
    captures: &[u32],
    names: Option<Names>,
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
                b'<' if let Some(mut names) = names
                    && let Some(close) =
                        replacement[index + 2..].iter().position(|&b| b == b'>') =>
                {
                    let name = &replacement[index + 2..index + 2 + close];
                    if let Some((group, _)) = names.find(|(_, candidate)| *candidate == name)
                        && let Some(range) = capture(captures, group)
                    {
                        out.extend_from_slice(&text[range]);
                    }
                    index += close + 3;
                }
                b'0'..=b'9' if group_count > 0 => {
                    let d1 = (next - b'0') as usize;
                    if let Some(&d2_ch @ b'0'..=b'9') = replacement.get(index + 2) {
                        let two = d1 * 10 + (d2_ch - b'0') as usize;
                        if two >= 1 && two <= group_count {
                            if let Some(range) = capture(captures, two) {
                                out.extend_from_slice(&text[range]);
                            }
                            index += 3;
                            continue;
                        }
                    }
                    if d1 >= 1 && d1 <= group_count {
                        if let Some(range) = capture(captures, d1) {
                            out.extend_from_slice(&text[range]);
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
        f64::from(runtime::scalars_length(&text[..start]))
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
    let mut matches = unsafe { matches(text, program) };
    let Some(first) = matches.next() else {
        return string;
    };
    let rep = unsafe { runtime::bytes(replacement) };
    let needs_captures = rep.contains(&b'$') && unsafe { group_count(program) } > 1;
    let mut cap_slots = [0u32; MAX_SLOTS];
    let global = unsafe { is_global(program) };
    let mut out = runtime::Buffer::new();
    let mut last_end = 0;
    for (start, end) in core::iter::once(first).chain(matches) {
        out.extend_from_slice(&text[last_end..start]);
        let cap_len = if needs_captures {
            unsafe { extract_captures(program, text, start, end, &mut cap_slots) * 2 }
        } else {
            0
        };
        substitute(
            &mut out,
            text,
            start,
            end,
            rep,
            &cap_slots[..cap_len],
            unsafe { names(program) },
        );
        last_end = end;
        if !global {
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
    unsafe { std_regex_exec_from(string, program, 0.0) }
}

/// # Safety
/// `string` is live, `program` is a compiler table, and `from` is a byte boundary.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_exec_from(
    string: *mut u8,
    program: *const u8,
    from: f64,
) -> *mut u8 {
    let text = unsafe { runtime::bytes(string) };
    let Some((start, end)) = (unsafe { next_match(text, from as usize, program) }) else {
        return core::ptr::null_mut();
    };
    let group_count = unsafe { group_count(program) };
    let mut cap_slots = [0u32; MAX_SLOTS];
    if group_count > 1 {
        unsafe { extract_captures(program, text, start, end, &mut cap_slots) };
    }
    // The normal array header is followed by immutable match metadata. Array mutations
    // replace its data buffer, leaving the metadata and its traced pointers intact.
    let capacity = group_count.max(4);
    let result = unsafe {
        runtime::alloc_shaped(
            RESULT_BYTES,
            &[0, 3, array::DATA, RESULT_GROUPS as u32, RESULT_INPUT as u32],
        )
    };
    let data =
        unsafe { runtime::alloc_shaped(capacity as u32 * array::SLOT, &[array::SLOT, 1, 0]) };
    unsafe {
        result
            .add(array::LEN as usize)
            .cast::<u32>()
            .write(group_count as u32);
        result
            .add(array::CAP as usize)
            .cast::<u32>()
            .write(capacity as u32);
        result
            .add(array::DATA as usize)
            .cast::<*mut u8>()
            .write(data);
        core::ptr::write_bytes(data, 0, capacity * array::SLOT as usize);
    }
    let slots = unsafe { core::slice::from_raw_parts_mut(data.cast::<u64>(), group_count) };
    slots[0] = unsafe { slice_string(string, text.len(), start, end) } as u64;
    for (i, slot) in slots.iter_mut().enumerate().skip(1) {
        *slot = capture(&cap_slots, i).map_or(0, |range| unsafe {
            slice_string(string, text.len(), range.start, range.end) as u64
        });
    }
    let mut groups = core::ptr::null_mut();
    if let Some(names) = unsafe { names(program) } {
        groups = crate::json::std_json_object();
        for (index, bytes) in names {
            let key = unsafe { runtime::adopt(bytes.as_ptr() as *mut u8, bytes.len() as u32) };
            let value = slots[index] as *mut u8;
            let value = if value.is_null() {
                value
            } else {
                unsafe { crate::json::std_json_from_string(value) }
            };
            unsafe { crate::json::std_json_set(groups, key, value) };
        }
    }
    let next = if end > start {
        end
    } else {
        next_scalar_boundary(text, end)
    };
    unsafe {
        result.add(RESULT_GROUPS).cast::<*mut u8>().write(groups);
        result
            .add(RESULT_INDEX)
            .cast::<f64>()
            .write(runtime::scalars_length(&text[..start]) as f64);
        result.add(RESULT_NEXT).cast::<f64>().write(next as f64);
        result.add(RESULT_INPUT).cast::<*mut u8>().write(string);
    }
    result
}

/// # Safety
/// `result` is an array returned by `std_regex_exec_from`, not an ordinary array.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_groups(result: *mut u8) -> *mut u8 {
    unsafe { result.add(RESULT_GROUPS).cast::<*mut u8>().read() }
}

/// # Safety
/// `result` is an array returned by `std_regex_exec_from`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_index(result: *mut u8) -> f64 {
    unsafe { result.add(RESULT_INDEX).cast::<f64>().read() }
}

/// # Safety
/// `result` is an array returned by `std_regex_exec_from`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_next(result: *mut u8) -> f64 {
    unsafe { result.add(RESULT_NEXT).cast::<f64>().read() }
}

/// # Safety
/// `result` is an array returned by `std_regex_exec_from`.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_input(result: *mut u8) -> *mut u8 {
    unsafe { result.add(RESULT_INPUT).cast::<*mut u8>().read() }
}

/// `s.match(regex)`.
///
/// # Safety
/// `string` is a live immutable string; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_match(string: *mut u8, program: *const u8) -> *mut u8 {
    if !unsafe { is_global(program) } {
        return unsafe { std_regex_exec(string, program) };
    }
    let text = unsafe { runtime::bytes(string) };
    let count = unsafe { matches(text, program) }.count();
    if count == 0 {
        return core::ptr::null_mut();
    }
    let (array, slots) = runtime::array_storage(count);
    for (slot, (start, end)) in slots.iter_mut().zip(unsafe { matches(text, program) }) {
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
