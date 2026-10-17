//! Statically checked reverse and anchored-forward DFA execution.
use crate::runtime;

/// Find the leftmost match `(start, end)` in `text` starting at or after byte offset `pos`.
///
/// # Safety
/// `program` must be a compiler-generated reverse+forward DFA table.
unsafe fn next_match(text: &[u8], pos: usize, program: *const u8) -> Option<(usize, usize)> {
    if pos > text.len() {
        return None;
    }
    let rev_end_column = unsafe { *program.cast::<u32>() } as usize;
    let rev_rows = unsafe { program.add(264) };
    let mut state = rev_rows;
    let mut best_start = None;
    for cursor in (pos..text.len()).rev() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *program.add(8 + byte as usize) };
        let offset = unsafe { *state.add(4 + usize::from(class) * 4).cast::<u32>() };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } != 0
            && text.get(cursor + 1).is_none_or(|&b| b & 0xc0 != 0x80)
        {
            best_start = Some(cursor + 1);
        }
    }
    if pos == 0 {
        let offset = unsafe { *state.add(rev_end_column).cast::<u32>() };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } != 0 {
            best_start = Some(0);
        }
    } else if let Some(&prev) = text.get(pos - 1) {
        let class = unsafe { *program.add(8 + prev as usize) };
        let offset = unsafe { *state.add(4 + usize::from(class) * 4).cast::<u32>() };
        state = unsafe { rev_rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } != 0 && text.get(pos).is_none_or(|&b| b & 0xc0 != 0x80)
        {
            best_start = Some(pos);
        }
    }
    let start = best_start?;

    let fwd_header = unsafe { *program.add(4).cast::<u32>() };
    let fwd_base = unsafe { program.add((fwd_header & 0x7fff_ffff) as usize) };
    let fwd_end_column = unsafe { *fwd_base.cast::<u32>() } as usize;
    let start_offset = if start == 0 {
        unsafe { *fwd_base.add(4).cast::<u32>() }
    } else if text
        .get(start - 1)
        .is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_')
    {
        unsafe { *fwd_base.add(8).cast::<u32>() }
    } else {
        unsafe { *fwd_base.add(12).cast::<u32>() }
    };
    let fwd_rows = unsafe { fwd_base.add(272) };
    let mut state = unsafe { fwd_rows.add(start_offset as usize) };
    let mut best_end = None;
    let mut hit_dead = false;
    for cursor in start..text.len() {
        let byte = unsafe { *text.get_unchecked(cursor) };
        let class = unsafe { *fwd_base.add(16 + byte as usize) };
        let offset = unsafe { *state.add(4 + usize::from(class) * 4).cast::<u32>() };
        state = unsafe { fwd_rows.add(offset as usize) };
        let flags = unsafe { *state.cast::<u32>() };
        if flags & 1 != 0 && (cursor == start || byte & 0xc0 != 0x80) {
            best_end = Some(cursor);
        }
        if flags & 2 != 0 {
            hit_dead = true;
            break;
        }
    }
    if !hit_dead {
        let offset = unsafe { *state.add(fwd_end_column).cast::<u32>() };
        state = unsafe { fwd_rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } & 1 != 0 {
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

pub(crate) fn push_substitution(
    out: &mut runtime::Buffer,
    text: &[u8],
    start: usize,
    end: usize,
    replacement: &[u8],
) {
    if !replacement.contains(&b'$') {
        out.extend_from_slice(replacement);
        return;
    }
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
        let offset = unsafe { *state.add(4 + usize::from(class) * 4).cast::<u32>() };
        state = unsafe { rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } != 0
            && text.get(cursor + 1).is_none_or(|&b| b & 0xc0 != 0x80)
        {
            best = Some(cursor + 1);
        }
    }
    let end_column = unsafe { *program.cast::<u32>() };
    let offset = unsafe { *state.add(end_column as usize).cast::<u32>() };
    state = unsafe { rows.add(offset as usize) };
    if unsafe { *state.cast::<u32>() } != 0 {
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
    let is_global = (unsafe { *program.add(4).cast::<u32>() } >> 31) != 0;
    let mut out = runtime::Buffer::new();
    out.extend_from_slice(text.get(..first_start).unwrap_or(&[]));
    push_substitution(&mut out, text, first_start, first_end, rep);
    if !is_global {
        out.extend_from_slice(text.get(first_end..).unwrap_or(&[]));
        return runtime::from_bytes(out.as_slice());
    }
    let mut last_end = first_end;
    let mut pos = if first_end > first_start {
        first_end
    } else if first_end < text.len() {
        next_scalar_boundary(text, first_end)
    } else {
        out.extend_from_slice(text.get(last_end..).unwrap_or(&[]));
        return runtime::from_bytes(out.as_slice());
    };
    while let Some((start, end)) = unsafe { next_match(text, pos, program) } {
        out.extend_from_slice(text.get(last_end..start).unwrap_or(&[]));
        push_substitution(&mut out, text, start, end, rep);
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
    runtime::from_bytes(out.as_slice())
}

/// `s.split(regex)`.
///
/// # Safety
/// `string` is a live immutable string; `program` is a DFA table.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn std_regex_split(
    string: *mut u8,
    program: *const u8,
) -> *mut u8 {
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
        let piece = if start == 0 && end == text.len() {
            string
        } else if start == end {
            runtime::from_static("")
        } else {
            unsafe { runtime::string_view(string, start as u32, end as u32) }
        };
        *slot = piece as u64;
    }
    array
}
