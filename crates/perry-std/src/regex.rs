//! Allocation-free search of a statically checked reverse DFA.
use crate::runtime;

/// # Safety
/// `text` is a live immutable string; `program` is a compiler-generated DFA table.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn std_regex_search(text: *mut u8, program: *const u8) -> f64 {
    let text = unsafe { runtime::bytes(text) };
    let rows = unsafe { program.add(260) };
    let mut state = rows;
    let mut best = None;
    for cursor in (0..text.len()).rev() {
        let class = unsafe { *program.add(4 + text[cursor] as usize) };
        let offset = unsafe { *state.add(4 + usize::from(class) * 4).cast::<u32>() };
        state = unsafe { rows.add(offset as usize) };
        if unsafe { *state.cast::<u32>() } != 0
            && (cursor + 1 == text.len() || text[cursor + 1] & 0xc0 != 0x80)
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
        text[..end]
            .iter()
            .filter(|&&byte| byte & 0xc0 != 0x80)
            .count() as f64
    })
}
