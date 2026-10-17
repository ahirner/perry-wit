//! Bounded literal regexes compiled into immutable reverse and forward DFA tables.
mod syntax;
use anyhow::{Context, Result, ensure};
use regex_automata::{
    Anchored, Input, MatchKind,
    dfa::{Automaton, StartKind, dense},
    nfa::thompson,
};
use std::collections::{BTreeMap, VecDeque};

pub(super) fn compile(pattern: &str, flags: &str) -> Result<Vec<u8>> {
    let is_global = flags.contains('g');
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

    // Anchored forward DFA with LeftmostFirst semantics to find the exact match end.
    let fwd_dfa = dense::Builder::new()
        .configure(
            dense::Config::new()
                .match_kind(MatchKind::LeftmostFirst)
                .start_kind(StartKind::Anchored)
                .dfa_size_limit(Some(4 * 1024 * 1024))
                .determinize_size_limit(Some(8 * 1024 * 1024)),
        )
        .thompson(thompson::Config::new().nfa_size_limit(Some(1024 * 1024)))
        .build(&pattern)
        .context("Unsupported or oversized literal regex")?;
    let mut fwd_classes = [0u8; 256];
    for (byte, class) in fwd_classes.iter_mut().enumerate() {
        *class = fwd_dfa.byte_classes().get(byte as u8);
    }
    let fwd_class_count = fwd_dfa.byte_classes().alphabet_len() - 1;
    let mut fwd_representatives = vec![0u8; fwd_class_count];
    for (byte, &class) in fwd_classes.iter().enumerate() {
        fwd_representatives[usize::from(class)] = byte as u8;
    }
    let start_bof = fwd_dfa.start_state_forward(&Input::new(b"").anchored(Anchored::Yes))?;
    let start_word = fwd_dfa
        .start_state_forward(&Input::new(&b"a"[..]).range(1..1).anchored(Anchored::Yes))?;
    let start_nonword = fwd_dfa
        .start_state_forward(&Input::new(&b" "[..]).range(1..1).anchored(Anchored::Yes))?;
    let mut fwd_ids = BTreeMap::new();
    let mut fwd_pending = VecDeque::new();
    for s in [start_bof, start_word, start_nonword] {
        let next_id = u32::try_from(fwd_ids.len())?;
        fwd_ids.entry(s).or_insert_with(|| {
            fwd_pending.push_back(s);
            next_id
        });
    }
    let mut fwd_rows = Vec::new();
    let fwd_stride = u32::try_from((fwd_class_count + 2) * 4)?;
    while let Some(state) = fwd_pending.pop_front() {
        ensure!(
            !fwd_dfa.is_quit_state(state),
            "Regex requires an unsupported runtime fallback"
        );
        let state_flags =
            u32::from(fwd_dfa.is_match_state(state)) | (u32::from(fwd_dfa.is_dead_state(state)) << 1);
        fwd_rows.push(state_flags);
        for next in fwd_representatives
            .iter()
            .map(|&byte| fwd_dfa.next_state(state, byte))
            .chain([fwd_dfa.next_eoi_state(state)])
        {
            let new_id = u32::try_from(fwd_ids.len())?;
            let id = *fwd_ids.entry(next).or_insert_with(|| {
                fwd_pending.push_back(next);
                new_id
            });
            fwd_rows.push(
                id.checked_mul(fwd_stride)
                    .context("Regex state offset overflow")?,
            );
        }
    }

    let fwd_offset = u32::try_from(8 + 256 + rows.len() * 4)?;
    let fwd_header = fwd_offset | (u32::from(is_global) << 31);
    let mut bytes = ((class_count as u32 + 1) * 4).to_le_bytes().to_vec();
    bytes.extend(fwd_header.to_le_bytes());
    bytes.extend(classes);
    bytes.extend(rows.into_iter().flat_map(u32::to_le_bytes));
    bytes.extend(((fwd_class_count as u32 + 1) * 4).to_le_bytes());
    bytes.extend((fwd_ids[&start_bof] * fwd_stride).to_le_bytes());
    bytes.extend((fwd_ids[&start_word] * fwd_stride).to_le_bytes());
    bytes.extend((fwd_ids[&start_nonword] * fwd_stride).to_le_bytes());
    bytes.extend(fwd_classes);
    bytes.extend(fwd_rows.into_iter().flat_map(u32::to_le_bytes));
    Ok(bytes)
}
