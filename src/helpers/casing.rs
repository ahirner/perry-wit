//! Allocation-free Unicode default casing, including expansions and final sigma.

use super::case_properties::{CASE_IGNORABLE, CASED};
use core::{
    char::{ToLowercase, ToUppercase},
    iter::{Once, once},
};

pub(super) fn mapped_scalars<I: Iterator<Item = char> + Clone>(
    input: I,
    upper: bool,
) -> impl Iterator<Item = char> {
    CaseMapping {
        input,
        upper,
        preceded_by_cased: false,
        expansion: None,
    }
}

struct CaseMapping<I> {
    input: I,
    upper: bool,
    preceded_by_cased: bool,
    expansion: Option<Expansion>,
}

enum Expansion {
    Lower(ToLowercase),
    Upper(ToUppercase),
    FinalSigma(Once<char>),
}

impl Iterator for Expansion {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self {
            Self::Lower(chars) => chars.next(),
            Self::Upper(chars) => chars.next(),
            Self::FinalSigma(chars) => chars.next(),
        }
    }
}

impl<I: Iterator<Item = char> + Clone> Iterator for CaseMapping<I> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        if let Some(scalar) = self.expansion.as_mut().and_then(Iterator::next) {
            return Some(scalar);
        }
        let scalar = self.input.next()?;
        let expansion = if self.upper {
            Expansion::Upper(scalar.to_uppercase())
        } else if scalar == 'Σ'
            && self.preceded_by_cased
            && !self
                .input
                .clone()
                .find(|&c| !has_property(c, CASE_IGNORABLE))
                .is_some_and(|c| has_property(c, CASED))
        {
            Expansion::FinalSigma(once('ς'))
        } else {
            Expansion::Lower(scalar.to_lowercase())
        };
        if !has_property(scalar, CASE_IGNORABLE) {
            self.preceded_by_cased = has_property(scalar, CASED);
        }
        self.expansion = Some(expansion);
        self.expansion.as_mut().and_then(Iterator::next)
    }
}

fn has_property(scalar: char, ranges: &[(u32, u32)]) -> bool {
    let value = scalar as u32;
    let index = ranges.partition_point(|&(_, end)| end < value);
    ranges.get(index).is_some_and(|&(start, _)| start <= value)
}
