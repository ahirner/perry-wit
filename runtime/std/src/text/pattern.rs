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
    find: impl Fn(usize) -> Option<(usize, usize)> + 'a,
) -> impl Iterator<Item = (usize, usize)> + 'a {
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

    pub(crate) fn split<C: IntoIterator<Item = *mut u8>>(
        &self,
        mut matches: impl Iterator<Item = (usize, usize)>,
        capacity: u32,
        mut captures: impl FnMut(usize, usize) -> C,
    ) -> *mut u8 {
        if self.text.is_empty() && matches.next().is_some() {
            return runtime::array([]);
        }
        let mut pieces = runtime::ReferenceArray::with_capacity(capacity);
        let mut previous = 0;
        for (start, end) in matches.take_while(|&(start, _)| start < self.text.len()) {
            if end == previous {
                continue;
            }
            pieces.push(self.piece(previous, start));
            for capture in captures(start, end) {
                pieces.push(capture);
            }
            previous = end;
        }
        pieces.push(self.piece(previous, self.text.len()));
        pieces.finish()
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
