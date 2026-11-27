//! Non-overlapping match ranges consumed by string and regex methods.
use super::{View, ascii_character};
use crate::runtime;

pub(crate) fn next_boundary(text: &[u8], pos: usize) -> usize {
    let mut next = pos + 1;
    while text.get(next).is_some_and(|b| b & 0xc0 == 0x80) {
        next += 1;
    }
    next
}

pub(crate) fn matches<'a>(
    text: &'a [u8],
    find: impl Fn(usize) -> Option<(usize, usize)> + Clone + 'a,
) -> impl Iterator<Item = (usize, usize)> + Clone + 'a {
    let mut from = 0;
    core::iter::from_fn(move || {
        if from > text.len() {
            return None;
        }
        let (start, end) = find(from)?;
        from = if start == end {
            next_boundary(text, end)
        } else {
            end
        };
        Some((start, end))
    })
}

impl View {
    pub(crate) fn piece(&self, start: usize, end: usize) -> *mut u8 {
        if start == 0 && end == self.text.len() {
            self.string
        } else if end == start {
            runtime::from_static(perry_rt_sys::static_text!(""))
        } else if end - start == 1 && self.text.as_bytes()[start].is_ascii() {
            ascii_character(self.text.as_bytes()[start])
        } else {
            unsafe { runtime::string_view(self.string, start as u32, end as u32) }
        }
    }

    pub(crate) fn split(
        &self,
        mut matches: impl Iterator<Item = (usize, usize)> + Clone,
        captures: usize,
        mut write_captures: impl FnMut(usize, usize, &mut [u64]),
    ) -> *mut u8 {
        if self.text.is_empty() && matches.next().is_some() {
            return runtime::array_storage(0).0;
        }
        let mut last_end = 0;
        let separators = matches
            .take_while(|&(start, _)| start < self.text.len())
            .filter(move |&(_, end)| core::mem::replace(&mut last_end, end) != end);
        let width = 1 + captures;
        let count = separators.clone().count();
        let len = count
            .checked_mul(width)
            .and_then(|n| n.checked_add(1))
            .expect("split array length");
        let (array, slots) = runtime::array_storage(len);
        let (suffix, pieces) = slots.split_last_mut().expect("split suffix");
        let mut previous = 0;
        for (chunk, (start, end)) in pieces.chunks_exact_mut(width).zip(separators) {
            chunk[0] = self.piece(previous, start) as u64;
            write_captures(start, end, &mut chunk[1..]);
            previous = end;
        }
        *suffix = self.piece(previous, self.text.len()) as u64;
        array
    }

    pub(crate) fn replace(
        &self,
        mut matches: impl Iterator<Item = (usize, usize)>,
        global: bool,
        mut substitute: impl FnMut(&mut runtime::Buffer, usize, usize),
    ) -> *mut u8 {
        let Some(first) = matches.next() else {
            return self.string;
        };
        let mut out = runtime::Buffer::new();
        let text = self.text.as_bytes();
        let mut previous = 0;
        for (start, end) in core::iter::once(first).chain(matches) {
            out.extend_from_slice(&text[previous..start]);
            substitute(&mut out, start, end);
            previous = end;
            if !global {
                break;
            }
        }
        out.extend_from_slice(&text[previous..]);
        unsafe { out.into_string() }
    }
}
