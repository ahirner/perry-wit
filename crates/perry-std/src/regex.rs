//! Statically checked reverse and anchored-forward DFA execution with optional NFA captures.
use crate::runtime;

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

fn look_matches(code: u8, text: &[u8], cursor: usize) -> bool {
    let prev_word = cursor > 0 && text.get(cursor - 1).is_some_and(|&b| is_word_byte(b));
    let curr_word = text.get(cursor).is_some_and(|&b| is_word_byte(b));
    if code == 1 {
        cursor == 0
    } else if code == 2 {
        cursor == text.len()
    } else if code == 3 {
        cursor == 0 || text.get(cursor - 1) == Some(&b'\n')
    } else if code == 4 {
        cursor == text.len() || text.get(cursor) == Some(&b'\n')
    } else if code == 5 {
        prev_word != curr_word
    } else if code == 6 {
        prev_word == curr_word
    } else if code == 7 {
        !prev_word && curr_word
    } else if code == 8 {
        prev_word && !curr_word
    } else {
        false
    }
}

const MAX_THREADS: usize = 24;
const MAX_SLOTS: usize = 16;

#[allow(clippy::too_many_arguments)]
unsafe fn eps_closure(
    nfa_base: *const u8,
    state_count: u16,
    states_base: *const u8,
    text: &[u8],
    end: usize,
    cursor: usize,
    seed_state: u16,
    work_slots: &mut [u32; MAX_SLOTS],
    visited: &mut [u64; 4],
    next_states: &mut [u16; MAX_THREADS],
    next_slots: &mut [[u32; MAX_SLOTS]; MAX_THREADS],
    next_len: &mut usize,
    matched_slots: &mut [u32; MAX_SLOTS],
) -> bool {
    let mut stack = [(0u32, 0u32); 64];
    let mut stack_len = 1usize;
    stack[0] = (u32::from(seed_state), 0);
    while stack_len > 0 {
        stack_len -= 1;
        let (tag, prev_val) = stack[stack_len];
        if tag & 0x8000_0000 != 0 {
            let idx = (tag & 0x7fff_ffff) as usize;
            if idx < MAX_SLOTS {
                work_slots[idx] = prev_val;
            }
            continue;
        }
        let state_id = tag as u16;
        if state_id >= state_count {
            continue;
        }
        let word = (state_id >> 6) as usize;
        let bit = 1u64 << (state_id & 63);
        if let Some(w) = visited.get_mut(word) {
            if *w & bit != 0 {
                continue;
            }
            *w |= bit;
        }
        let offset = unsafe { read_u16(nfa_base.add(4 + usize::from(state_id) * 2)) } as usize;
        let sp = unsafe { states_base.add(offset) };
        let op = unsafe { *sp };
        if op == 1 || op == 2 {
            if *next_len < MAX_THREADS {
                next_states[*next_len] = state_id;
                next_slots[*next_len] = *work_slots;
                *next_len += 1;
            }
        } else if op == 3 {
            let code = unsafe { *sp.add(1) };
            let next = unsafe { read_u16(sp.add(2)) };
            if look_matches(code, text, cursor) && stack_len < stack.len() {
                stack[stack_len] = (u32::from(next), 0);
                stack_len += 1;
            }
        } else if op == 4 {
            let count = unsafe { read_u16(sp.add(1)) } as usize;
            let ap = unsafe { sp.add(3) };
            for i in (0..count).rev() {
                if stack_len < stack.len() {
                    let alt = unsafe { read_u16(ap.add(i * 2)) };
                    stack[stack_len] = (u32::from(alt), 0);
                    stack_len += 1;
                }
            }
        } else if op == 5 {
            let raw_slot = unsafe { *sp.add(1) } as usize;
            let next = unsafe { read_u16(sp.add(2)) };
            if let Some(idx) = raw_slot.checked_sub(2)
                && idx < MAX_SLOTS
            {
                let prev = work_slots[idx];
                work_slots[idx] = cursor as u32 + 1;
                if stack_len + 2 <= stack.len() {
                    stack[stack_len] = ((idx as u32) | 0x8000_0000, prev);
                    stack[stack_len + 1] = (u32::from(next), 0);
                    stack_len += 2;
                }
            } else if stack_len < stack.len() {
                stack[stack_len] = (u32::from(next), 0);
                stack_len += 1;
            }
        } else if op == 6 && cursor == end {
            *matched_slots = *work_slots;
            return true;
        }
    }
    false
}

unsafe fn extract_captures(
    program: *const u8,
    text: &[u8],
    start: usize,
    end: usize,
    matched_slots: &mut [u32; MAX_SLOTS],
) -> usize {
    *matched_slots = [0u32; MAX_SLOTS];
    let group_count = (unsafe { read_u16(program.add(2)) } as usize).min(1 + MAX_SLOTS / 2);
    if group_count <= 1 {
        return 0;
    }
    let fwd_header = unsafe { read_u32(program.add(4)) };
    let fwd_base = unsafe { program.add((fwd_header & 0x7fff_ffff) as usize) };
    let nfa_offset = unsafe { read_u32(fwd_base.add(8)) } as usize;
    if nfa_offset == 0 {
        return 0;
    }
    let nfa_base = unsafe { program.add(nfa_offset) };
    let state_count = unsafe { read_u16(nfa_base) };
    let start_state = unsafe { read_u16(nfa_base.add(2)) };
    let states_base = unsafe { nfa_base.add(4 + usize::from(state_count) * 2) };
    let mut curr_states = [0u16; MAX_THREADS];
    let mut next_states = [0u16; MAX_THREADS];
    let mut curr_slots = [[0u32; MAX_SLOTS]; MAX_THREADS];
    let mut next_slots = [[0u32; MAX_SLOTS]; MAX_THREADS];
    let mut curr_len = 0usize;
    let mut work_slots = [0u32; MAX_SLOTS];
    let mut visited = [0u64; 4];
    let _ = unsafe {
        eps_closure(
            nfa_base,
            state_count,
            states_base,
            text,
            end,
            start,
            start_state,
            &mut work_slots,
            &mut visited,
            &mut curr_states,
            &mut curr_slots,
            &mut curr_len,
            matched_slots,
        )
    };
    for cursor in start..end {
        if curr_len == 0 {
            break;
        }
        let b = unsafe { *text.get_unchecked(cursor) };
        let mut next_len = 0usize;
        visited = [0u64; 4];
        for i in 0..curr_len {
            let state_id = curr_states[i];
            let offset = unsafe { read_u16(nfa_base.add(4 + usize::from(state_id) * 2)) } as usize;
            let sp = unsafe { states_base.add(offset) };
            let op = unsafe { *sp };
            let target = if op == 1 {
                let lo = unsafe { *sp.add(1) };
                let hi = unsafe { *sp.add(2) };
                if b >= lo && b <= hi {
                    Some(unsafe { read_u16(sp.add(3)) })
                } else {
                    None
                }
            } else if op == 2 {
                let count = unsafe { read_u16(sp.add(1)) } as usize;
                let mut tp = unsafe { sp.add(3) };
                let mut found = None;
                for _ in 0..count {
                    let lo = unsafe { *tp };
                    let hi = unsafe { *tp.add(1) };
                    if b >= lo && b <= hi {
                        found = Some(unsafe { read_u16(tp.add(2)) });
                        break;
                    }
                    tp = unsafe { tp.add(4) };
                }
                found
            } else {
                None
            };
            if let Some(next_state) = target {
                work_slots = curr_slots[i];
                if unsafe {
                    eps_closure(
                        nfa_base,
                        state_count,
                        states_base,
                        text,
                        end,
                        cursor + 1,
                        next_state,
                        &mut work_slots,
                        &mut visited,
                        &mut next_states,
                        &mut next_slots,
                        &mut next_len,
                        matched_slots,
                    )
                } {
                    break;
                }
            }
        }
        curr_states = next_states;
        curr_slots = next_slots;
        curr_len = next_len;
    }
    group_count - 1
}

/// Find the leftmost match `(start, end)` in `text` starting at or after byte offset `pos`.
///
/// # Safety
/// `program` must be a compiler-generated reverse+forward DFA table.
unsafe fn next_match(text: &[u8], pos: usize, program: *const u8) -> Option<(usize, usize)> {
    if pos > text.len() {
        return None;
    }
    let rev_end_column = unsafe { read_u16(program) } as usize;
    let rev_rows = unsafe { program.add(264) };
    let mut state = rev_rows;
    let mut best_start = None;
    for cursor in (pos..text.len()).rev() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *program.add(8 + byte as usize) };
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
        let class = unsafe { *program.add(8 + prev as usize) };
        let offset = unsafe { read_u16(state.add(2 + usize::from(class) * 2)) };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { read_u16(state) } != 0 && text.get(pos).is_none_or(|&b| b & 0xc0 != 0x80) {
            best_start = Some(pos);
        }
    }
    let start = best_start?;

    let fwd_header = unsafe { read_u32(program.add(4)) };
    let fwd_base = unsafe { program.add((fwd_header & 0x7fff_ffff) as usize) };
    let fwd_end_column = unsafe { read_u16(fwd_base) } as usize;
    let start_offset = if start == 0 {
        unsafe { read_u16(fwd_base.add(2)) }
    } else if text.get(start - 1).is_some_and(|&b| is_word_byte(b)) {
        unsafe { read_u16(fwd_base.add(4)) }
    } else {
        unsafe { read_u16(fwd_base.add(6)) }
    };
    let fwd_rows = unsafe { fwd_base.add(268) };
    let mut state = unsafe { fwd_rows.add(start_offset as usize) };
    let mut best_end = None;
    let mut hit_dead = false;
    for cursor in start..text.len() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *fwd_base.add(12 + byte as usize) };
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
                    out.extend_from_slice(text.get(start..end).unwrap_or(&[]));
                    index += 2;
                }
                b'`' => {
                    out.extend_from_slice(text.get(..start).unwrap_or(&[]));
                    index += 2;
                }
                b'\'' => {
                    out.extend_from_slice(text.get(end..).unwrap_or(&[]));
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
                                out.extend_from_slice(
                                    text.get((s as usize - 1)..(e as usize - 1)).unwrap_or(&[]),
                                );
                            }
                            index += 3;
                            continue;
                        }
                    }
                    if d1 >= 1 && d1 <= group_count {
                        let s = captures[(d1 - 1) * 2];
                        let e = captures[(d1 - 1) * 2 + 1];
                        if s != 0 && e != 0 && s <= e {
                            out.extend_from_slice(
                                text.get((s as usize - 1)..(e as usize - 1)).unwrap_or(&[]),
                            );
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
    let rows = unsafe { program.add(264) };
    let mut state = rows;
    let mut best = None;
    for cursor in (0..text.len()).rev() {
        let class = unsafe { *program.add(8 + *text.get_unchecked(cursor) as usize) };
        let offset = unsafe { read_u16(state.add(2 + usize::from(class) * 2)) };
        state = unsafe { rows.add(offset as usize) };
        if unsafe { read_u16(state) } != 0 && text.get(cursor + 1).is_none_or(|&b| b & 0xc0 != 0x80)
        {
            best = Some(cursor + 1);
        }
    }
    let end_column = unsafe { read_u16(program) };
    let offset = unsafe { read_u16(state.add(end_column as usize)) };
    state = unsafe { rows.add(offset as usize) };
    if unsafe { read_u16(state) } != 0 {
        best = Some(0);
    }
    best.map_or(-1.0, |end| {
        text.get(..end)
            .unwrap_or(&[])
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
    let needs_captures = rep.contains(&b'$') && unsafe { read_u16(program.add(2)) } > 1;
    let mut cap_slots = [0u32; MAX_SLOTS];
    let is_global = (unsafe { read_u32(program.add(4)) } >> 31) != 0;
    let mut out = runtime::Buffer::new();
    out.extend_from_slice(text.get(..first_start).unwrap_or(&[]));
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
        out.extend_from_slice(text.get(first_end..).unwrap_or(&[]));
        return unsafe { out.into_string() };
    }
    let mut last_end = first_end;
    let mut pos = if first_end > first_start {
        first_end
    } else if first_end < text.len() {
        next_scalar_boundary(text, first_end)
    } else {
        out.extend_from_slice(text.get(last_end..).unwrap_or(&[]));
        return unsafe { out.into_string() };
    };
    while let Some((start, end)) = unsafe { next_match(text, pos, program) } {
        out.extend_from_slice(text.get(last_end..start).unwrap_or(&[]));
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
    out.extend_from_slice(text.get(last_end..).unwrap_or(&[]));
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
    let group_count = (unsafe { read_u16(program.add(2)) } as usize).clamp(1, 1 + MAX_SLOTS / 2);
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
