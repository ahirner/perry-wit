use core::fmt::Write;
use icu_normalizer::properties::{
    CanonicalCompositionBorrowed, CanonicalDecompositionBorrowed, Decomposed,
};
use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn core::error::Error>> {
    let decomposition = CanonicalDecompositionBorrowed::new();
    let composition = CanonicalCompositionBorrowed::new();
    let mut pairs = Vec::<(char, char, char, char, bool)>::new();
    let mut generated = String::from("static SINGLETONS: &[(char, char)] = &[\n");
    for ch in (0..=0x10ffff).filter_map(char::from_u32) {
        let (first, second) = match decomposition.decompose(ch) {
            Decomposed::Default => continue,
            Decomposed::Singleton(first) => {
                writeln!(generated, "({ch:?}, {first:?}),")?;
                continue;
            }
            Decomposed::Expansion(first, second) => (first, second),
        };
        let composes = composition.compose(first, second) == Some(ch);
        if let Some((start, end, previous_first, previous_second, previous_composes)) =
            pairs.last_mut()
            && *end as u32 + 1 == ch as u32
            && *previous_first == first
            && *previous_composes == composes
            && *previous_second as u32 + ch as u32 - *start as u32 == second as u32
        {
            *end = ch;
        } else {
            pairs.push((ch, ch, first, second, composes));
        }
    }
    let mut compositions: Vec<_> = (0..pairs.len()).filter(|&index| pairs[index].4).collect();
    compositions.sort_unstable_by_key(|&index| (pairs[index].2, pairs[index].3));
    assert!(pairs.len() <= u16::MAX as usize);
    generated.push_str("];\nstatic PAIRS: &[CanonicalPairRange] = &[\n");
    for (start, end, first, second, _) in &pairs {
        writeln!(
            generated,
            "CanonicalPairRange {{ start: {start:?}, end: {end:?}, first: {first:?}, second_start: {second:?} }},"
        )?;
    }
    generated.push_str("];\nstatic COMPOSITIONS: &[u16] = &[\n");
    for index in compositions {
        writeln!(generated, "{index},")?;
    }
    generated.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").ok_or("Cargo did not set OUT_DIR")?)
            .join("canonical.rs"),
        generated,
    )?;
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
