//! Dense bitsets backed by `Vec<u64>`, shared across boundary facts, worklists, and liveness.

#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub(crate) struct BitSet {
    words: Vec<u64>,
}

impl BitSet {
    pub(crate) fn clear(&mut self) {
        self.words.fill(0);
    }

    pub(crate) fn insert(&mut self, bit: usize) -> bool {
        let word_idx = bit / 64;
        let mask = 1u64 << (bit % 64);
        if word_idx >= self.words.len() {
            self.words.resize(word_idx + 1, 0);
        }
        let Some(word) = self.words.get_mut(word_idx) else {
            return false;
        };
        let was_set = (*word & mask) != 0;
        *word |= mask;
        !was_set
    }

    pub(crate) fn remove(&mut self, bit: usize) -> bool {
        let Some(word) = self.words.get_mut(bit / 64) else {
            return false;
        };
        let mask = 1u64 << (bit % 64);
        let was_set = (*word & mask) != 0;
        *word &= !mask;
        was_set
    }

    pub(crate) fn contains(&self, bit: usize) -> bool {
        self.words
            .get(bit / 64)
            .is_some_and(|word| (*word & (1u64 << (bit % 64))) != 0)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.words.iter().all(|&w| w == 0)
    }

    pub(crate) fn union_with(&mut self, other: &Self) -> bool {
        if other.words.len() > self.words.len() {
            self.words.resize(other.words.len(), 0);
        }
        let mut changed = false;
        for (dst, &src) in self.words.iter_mut().zip(&other.words) {
            let next = *dst | src;
            if next != *dst {
                *dst = next;
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn iter(&self) -> BitSetIter<'_> {
        BitSetIter {
            words: &self.words,
            word_idx: 0,
            current: self.words.first().copied().unwrap_or(0),
        }
    }
}

pub(crate) struct BitSetIter<'a> {
    words: &'a [u64],
    word_idx: usize,
    current: u64,
}

impl Iterator for BitSetIter<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        while self.current == 0 {
            self.word_idx += 1;
            self.current = *self.words.get(self.word_idx)?;
        }
        let tz = self.current.trailing_zeros() as usize;
        self.current &= self.current.wrapping_sub(1);
        Some(self.word_idx * 64 + tz)
    }
}
