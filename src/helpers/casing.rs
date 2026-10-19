//! Allocation-free Unicode default casing, including expansions and final sigma.

use core::{
    char::{ToLowercase, ToUppercase},
    iter::{Once, once},
};
use icu_properties::{
    CodePointSetData,
    props::{CaseIgnorable, Cased},
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
                .find(|&c| !is_case_ignorable(c))
                .is_some_and(is_cased)
        {
            Expansion::FinalSigma(once('ς'))
        } else {
            Expansion::Lower(scalar.to_lowercase())
        };
        if !self.upper && !is_case_ignorable(scalar) {
            self.preceded_by_cased = is_cased(scalar);
        }
        self.expansion = Some(expansion);
        self.expansion.as_mut().and_then(Iterator::next)
    }
}

fn is_case_ignorable(scalar: char) -> bool {
    if scalar.is_ascii() {
        matches!(scalar, '\'' | '.' | ':' | '^' | '`')
    } else {
        CodePointSetData::new::<CaseIgnorable>().contains(scalar)
    }
}

fn is_cased(scalar: char) -> bool {
    if scalar.is_ascii() {
        scalar.is_ascii_alphabetic()
    } else {
        CodePointSetData::new::<Cased>().contains(scalar)
    }
}
