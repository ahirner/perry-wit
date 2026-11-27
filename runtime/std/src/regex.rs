//! Statically checked forward and anchored-reverse DFA execution with optional NFA captures.
use crate::runtime;
use perry_rt_sys::layout::array;
use perry_rt_sys::layout::regex::{
    CAPTURE_PROGRAM, DFA_CLASSES, DFA_ROWS, GLOBAL, GROUP_COUNT, LookCode, MAX_CAPTURES,
    MAX_PENDING, MAX_STATES, MAX_THREADS, NAMES_OFFSET, NfaState, NfaTable, PROGRAM_HEADER,
    REVERSE_OFFSET,
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
    let count = unsafe { read_u16(program.add(GROUP_COUNT)) } as usize;
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

unsafe fn extract_captures(program: *const u8, text: &[u8], start: usize, end: usize) -> Slots {
    let nfa_offset = unsafe { read_u32(program.add(CAPTURE_PROGRAM)) } as usize;
    assert!(nfa_offset != 0, "missing regex capture program");
    let bytes = unsafe {
        let end = read_u32(program.add(NAMES_OFFSET)) as usize;
        core::slice::from_raw_parts(program.add(nfa_offset), end - nfa_offset)
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
    nfa.matched
}

/// # Safety
/// `program` must be a compiler-generated regex table.
unsafe fn is_global(program: *const u8) -> bool {
    unsafe { *program.add(GLOBAL) != 0 }
}

/// One direction of a compiled table: rows are `[flags: u16, next row offset per class: u16...]`.
#[derive(Clone, Copy)]
struct Dfa {
    base: *const u8,
    classes: *const u8,
    end_column: usize,
}

impl Dfa {
    /// # Safety
    /// `base` heads a compiler-generated DFA with a static byte-class map and state rows.
    unsafe fn new(base: *const u8) -> Self {
        unsafe {
            Self {
                base,
                classes: read_u32(base.add(DFA_CLASSES)) as *const u8,
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

    /// # Safety
    /// `column` of `state` holds a row offset of this table.
    unsafe fn goto(self, state: *const u8, column: usize) -> *const u8 {
        unsafe {
            self.base
                .add(DFA_ROWS + usize::from(read_u16(state.add(column))))
        }
    }

    /// DFA matches lag by one byte. The final transition consults the byte outside
    /// the search range, or end-of-input when the range reaches the edge of the text.
    unsafe fn find<const REVERSE: bool>(
        self,
        text: &[u8],
        start: usize,
        end: usize,
    ) -> Option<usize> {
        let (mut at, limit, adjacent) = if REVERSE {
            (end, start, text.get(end))
        } else {
            (start, end, start.checked_sub(1).and_then(|i| text.get(i)))
        };
        let column = match adjacent {
            None => 2,
            Some(&b) if is_word_byte(b) => 4,
            Some(_) => 6,
        };
        let mut state = unsafe { self.goto(self.base, column) };
        let mut found = None;
        loop {
            let byte = if REVERSE {
                at.checked_sub(1).and_then(|i| text.get(i))
            } else {
                text.get(at)
            };
            state = unsafe {
                match byte {
                    Some(&b) => self.step(state, b),
                    None => self.goto(state, self.end_column),
                }
            };
            let flags = unsafe { read_u16(state) };
            if flags & 1 != 0 {
                found = Some(at);
            }
            if at == limit || flags & 2 != 0 {
                return found;
            }
            if REVERSE {
                at -= 1;
            } else {
                at += 1;
            }
        }
    }
}

unsafe fn next_match(text: &[u8], mut pos: usize, program: *const u8) -> Option<(usize, usize)> {
    let forward = unsafe { Dfa::new(program.add(PROGRAM_HEADER)) };
    while pos <= text.len() {
        let end = unsafe { forward.find::<false>(text, pos, text.len()) }?;
        // Only empty matches can split a UTF-8 scalar. Resume at the next boundary.
        if text.get(end).is_some_and(|b| b & 0xc0 == 0x80) {
            pos = crate::text::pattern::next_boundary(text, end);
            continue;
        }
        let reverse = unsafe { read_u32(program.add(REVERSE_OFFSET)) } as i32;
        let start = if reverse < 0 {
            end - (!reverse) as usize
        } else if end == pos {
            pos
        } else {
            let reverse = unsafe { Dfa::new(program.add(reverse as usize)) };
            unsafe { reverse.find::<true>(text, pos, end) }?
        };
        return Some((start, end));
    }
    None
}

unsafe fn matches(text: &[u8], program: *const u8) -> impl Iterator<Item = (usize, usize)> {
    crate::text::pattern::matches(text, move |from| unsafe { next_match(text, from, program) })
}

/// Return the byte range of 1-based capture `group`. Slots store `offset + 1` so `0` represents an unset group.
fn capture(slots: &[u32], group: usize) -> Option<core::ops::Range<usize>> {
    let (s, e) = (slots[2 * group - 2], slots[2 * group - 1]);
    (s != 0 && e != 0 && s <= e).then(|| s as usize - 1..e as usize - 1)
}

pub(crate) fn push_substitution(
    out: &mut runtime::Buffer,
    text: &[u8],
    start: usize,
    end: usize,
    replacement: &[u8],
) {
    substitute(out, text, start, end, replacement, &[], None);
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
/// `text` is a live immutable string, and `program` is a compiler-generated DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_search(text: *mut u8, program: *const u8) -> f64 {
    let view = crate::text::view(text);
    unsafe { next_match(view.text.as_bytes(), 0, program) }
        .map_or(-1.0, |(start, _)| f64::from(view.position(start)))
}

/// `s.replace(regex, replacement)` and `s.replaceAll(regex, replacement)`.
///
/// # Safety
/// `string` and `replacement` are live immutable strings, and `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_replace(
    string: *mut u8,
    program: *const u8,
    replacement: *mut u8,
) -> *mut u8 {
    let view = crate::text::view(string);
    let text = view.text.as_bytes();
    let rep = unsafe { runtime::bytes(replacement) };
    let capture_count = if rep.contains(&b'$') {
        unsafe { group_count(program) - 1 }
    } else {
        0
    };
    let global = unsafe { is_global(program) };
    let names = unsafe { names(program) };
    view.replace(
        unsafe { matches(text, program) },
        global,
        |out, start, end| {
            let slots;
            let captures = if capture_count == 0 {
                &[]
            } else {
                slots = unsafe { extract_captures(program, text, start, end) };
                &slots[..2 * capture_count]
            };
            substitute(out, text, start, end, rep, captures, names);
        },
    )
}

unsafe fn capture_values(
    view: &crate::text::View,
    program: *const u8,
    start: usize,
    end: usize,
    count: usize,
) -> impl Iterator<Item = *mut u8> {
    let slots = if count == 0 {
        [0; MAX_SLOTS]
    } else {
        unsafe { extract_captures(program, view.text.as_bytes(), start, end) }
    };
    (1..=count).map(move |group| {
        capture(&slots, group).map_or(core::ptr::null_mut(), |range| {
            view.piece(range.start, range.end)
        })
    })
}

#[repr(C)]
struct MatchArray {
    array: array::Header,
    index: f64,
    input: *mut u8,
    groups: *mut u8,
}

/// `regex.exec(s)`.
///
/// # Safety
/// `string` is a live immutable string, and `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_exec(string: *mut u8, program: *const u8) -> *mut u8 {
    unsafe { std_regex_exec_from(string, program, 0.0) }
}

/// # Safety
/// `string` is live, `program` is a compiler table, and `from` is a scalar position.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_exec_from(
    string: *mut u8,
    program: *const u8,
    from: f64,
) -> *mut u8 {
    let view = crate::text::view(string);
    if from > view.scalars as f64 {
        return core::ptr::null_mut();
    }
    let text = view.text.as_bytes();
    let Some((start, end)) = (unsafe { next_match(text, view.offset(from as u64), program) })
    else {
        return core::ptr::null_mut();
    };
    let group_count = unsafe { group_count(program) };
    let cap = (group_count as u32).max(array::MIN_CAPACITY);
    let data =
        unsafe { runtime::alloc_shaped(cap * array::SLOT, &[array::SLOT, 1, 0]) }.cast::<u64>();
    let result = runtime::object!(MatchArray; array.data, array.holes, input, groups);
    unsafe {
        core::ptr::write_bytes(data, 0, cap as usize);
        result.write(MatchArray {
            array: array::Header {
                len: group_count as u32,
                cap,
                data,
                holes: core::ptr::null_mut(),
            },
            index: f64::from(view.position(start)),
            input: string,
            groups: core::ptr::null_mut(),
        });
    }
    let slots = unsafe { core::slice::from_raw_parts_mut(data, group_count) };
    slots[0] = view.piece(start, end) as u64;
    for (slot, value) in slots[1..]
        .iter_mut()
        .zip(unsafe { capture_values(&view, program, start, end, group_count - 1) })
    {
        *slot = value as u64;
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
    unsafe {
        (*result).groups = groups;
    }
    result.cast()
}

macro_rules! regex_result_fields {
    ($($name:ident => $field:ident: $ty:ty;)*) => {$(
        #[unsafe(no_mangle)]
        unsafe extern "C" fn $name(result: *const MatchArray) -> $ty {
            unsafe { (*result).$field }
        }
    )*};
}

regex_result_fields! {
    std_regex_groups => groups: *mut u8;
    std_regex_index => index: f64;
    std_regex_input => input: *mut u8;
}

/// `s.match(regex)`.
///
/// # Safety
/// `string` is a live immutable string, and `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_match(string: *mut u8, program: *const u8) -> *mut u8 {
    if !unsafe { is_global(program) } {
        return unsafe { std_regex_exec(string, program) };
    }
    let view = crate::text::view(string);
    let mut ranges = unsafe { matches(view.text.as_bytes(), program) }.peekable();
    if ranges.peek().is_none() {
        return core::ptr::null_mut();
    }
    runtime::array(ranges.map(|(start, end)| view.piece(start, end)))
}

/// `s.split(regex)`.
///
/// # Safety
/// `string` is a live immutable string, and `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_split(string: *mut u8, program: *const u8) -> *mut u8 {
    let view = crate::text::view(string);
    let ranges = unsafe { matches(view.text.as_bytes(), program) };
    let count = unsafe { group_count(program) - 1 };
    view.split(ranges, 0, |start, end| unsafe {
        capture_values(&view, program, start, end, count)
    })
}
