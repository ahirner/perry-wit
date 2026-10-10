//! Bounded literal regexes compiled into immutable reverse DFA tables.
mod syntax;
use anyhow::{Context, Result, ensure};
use regex_automata::{
    Input, MatchKind,
    dfa::{Automaton, dense},
    nfa::thompson,
};
use std::collections::{BTreeMap, VecDeque};

pub(super) fn compile(pattern: &str, flags: &str) -> Result<Vec<u8>> {
    let pattern = syntax::normalize(pattern, flags)?;
    // Search only needs the leftmost start. An overlapping reverse DFA reports
    // all possible starts; the final report is the earliest, independent of greed.
    let dfa = dense::Builder::new()
        .configure(
            dense::Config::new()
                .match_kind(MatchKind::All)
                .dfa_size_limit(Some(4 * 1024 * 1024))
                .determinize_size_limit(Some(8 * 1024 * 1024)),
        )
        .thompson(
            thompson::Config::new()
                .reverse(true)
                .nfa_size_limit(Some(1024 * 1024)),
        )
        .build(&pattern)
        .context("Unsupported or oversized literal regex")?;
    let mut classes = [0u8; 256];
    for (byte, class) in classes.iter_mut().enumerate() {
        *class = dfa.byte_classes().get(byte as u8);
    }
    // The alphabet counts one extra class for the end of input.
    let class_count = dfa.byte_classes().alphabet_len() - 1;
    let mut representatives = vec![0u8; class_count];
    for (byte, &class) in classes.iter().enumerate() {
        representatives[usize::from(class)] = byte as u8;
    }
    let start = dfa.start_state_reverse(&Input::new(b""))?;
    let mut ids = BTreeMap::from([(start, 0u32)]);
    let mut pending = VecDeque::from([start]);
    let mut rows = Vec::new();
    let stride = u32::try_from((class_count + 2) * 4)?;
    while let Some(state) = pending.pop_front() {
        ensure!(
            !dfa.is_quit_state(state),
            "Regex requires an unsupported runtime fallback"
        );
        rows.push(u32::from(dfa.is_match_state(state)));
        for next in representatives
            .iter()
            .map(|&byte| dfa.next_state(state, byte))
            .chain([dfa.next_eoi_state(state)])
        {
            let new_id = u32::try_from(ids.len())?;
            let id = *ids.entry(next).or_insert_with(|| {
                pending.push_back(next);
                new_id
            });
            rows.push(
                id.checked_mul(stride)
                    .context("Regex state offset overflow")?,
            );
        }
    }
    let mut bytes = ((class_count as u32 + 1) * 4).to_le_bytes().to_vec();
    bytes.extend(classes);
    bytes.extend(rows.into_iter().flat_map(u32::to_le_bytes));
    Ok(bytes)
}
