//! Bounded literal regexes compiled into immutable reverse and forward DFA tables.
mod syntax;
use anyhow::{Context, Result, bail, ensure};
use regex_automata::{
    Anchored, Input, MatchKind,
    dfa::{Automaton, StartKind, dense},
    nfa::thompson::{self, State},
    util::look::Look,
};
use std::collections::{BTreeMap, VecDeque};

pub(super) fn compile(pattern: &str, flags: &str) -> Result<Vec<u8>> {
    let is_global = flags.contains('g');
    let pattern = syntax::normalize(pattern, flags)?;
    let nfa = thompson::Compiler::new()
        .configure(thompson::Config::new().nfa_size_limit(Some(1024 * 1024)))
        .build(&pattern)
        .context("Unsupported or oversized literal regex")?;
    let group_count = u16::try_from(nfa.group_info().all_group_len())
        .context("Regex has too many capture groups")?;

    // Search only needs the leftmost start. An overlapping reverse DFA reports
    // all possible starts; the final report is the earliest, independent of greed.
    let dfa = dense::Builder::new()
        .configure(
            dense::Config::new()
                .minimize(true)
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
    let mut ids = BTreeMap::from([(start, 0u16)]);
    let mut pending = VecDeque::from([start]);
    let mut rows = Vec::<u16>::new();
    let stride = u16::try_from((class_count + 2) * 2).context("Regex state stride overflow")?;
    while let Some(state) = pending.pop_front() {
        ensure!(
            !dfa.is_quit_state(state),
            "Regex requires an unsupported runtime fallback"
        );
        rows.push(u16::from(dfa.is_match_state(state)));
        for next in representatives
            .iter()
            .map(|&byte| dfa.next_state(state, byte))
            .chain([dfa.next_eoi_state(state)])
        {
            let new_id = u16::try_from(ids.len()).context("Regex state count overflow")?;
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
                .minimize(true)
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
    let start_word =
        fwd_dfa.start_state_forward(&Input::new(&b"a"[..]).range(1..1).anchored(Anchored::Yes))?;
    let start_nonword =
        fwd_dfa.start_state_forward(&Input::new(&b" "[..]).range(1..1).anchored(Anchored::Yes))?;
    let mut fwd_ids = BTreeMap::new();
    let mut fwd_pending = VecDeque::new();
    for s in [start_bof, start_word, start_nonword] {
        let next_id = u16::try_from(fwd_ids.len()).context("Regex state count overflow")?;
        fwd_ids.entry(s).or_insert_with(|| {
            fwd_pending.push_back(s);
            next_id
        });
    }
    let mut fwd_rows = Vec::<u16>::new();
    let fwd_stride =
        u16::try_from((fwd_class_count + 2) * 2).context("Regex state stride overflow")?;
    while let Some(state) = fwd_pending.pop_front() {
        ensure!(
            !fwd_dfa.is_quit_state(state),
            "Regex requires an unsupported runtime fallback"
        );
        let state_flags = u16::from(fwd_dfa.is_match_state(state))
            | (u16::from(fwd_dfa.is_dead_state(state)) << 1);
        fwd_rows.push(state_flags);
        for next in fwd_representatives
            .iter()
            .map(|&byte| fwd_dfa.next_state(state, byte))
            .chain([fwd_dfa.next_eoi_state(state)])
        {
            let new_id = u16::try_from(fwd_ids.len()).context("Regex state count overflow")?;
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

    let rev_bytes_len = rows.len() * 2;
    let rev_padding = (4 - (rev_bytes_len & 3)) & 3;
    let fwd_offset = u32::try_from(8 + 256 + rev_bytes_len + rev_padding)?;
    let fwd_header = fwd_offset | (u32::from(is_global) << 31);
    let fwd_bytes_len = fwd_rows.len() * 2;
    let nfa_bytes = if group_count > 1 {
        Some(compile_nfa(&nfa)?)
    } else {
        None
    };
    let nfa_offset = if nfa_bytes.is_some() {
        fwd_offset
            .checked_add(u32::try_from(12 + 256 + fwd_bytes_len)?)
            .context("Regex NFA offset overflow")?
    } else {
        0
    };

    let mut bytes = Vec::new();
    bytes.extend(u16::try_from((class_count + 1) * 2)?.to_le_bytes());
    bytes.extend(group_count.to_le_bytes());
    bytes.extend(fwd_header.to_le_bytes());
    bytes.extend(classes);
    bytes.extend(rows.into_iter().flat_map(u16::to_le_bytes));
    bytes.resize(bytes.len() + rev_padding, 0);

    bytes.extend(u16::try_from((fwd_class_count + 1) * 2)?.to_le_bytes());
    bytes.extend((fwd_ids[&start_bof] * fwd_stride).to_le_bytes());
    bytes.extend((fwd_ids[&start_word] * fwd_stride).to_le_bytes());
    bytes.extend((fwd_ids[&start_nonword] * fwd_stride).to_le_bytes());
    bytes.extend(nfa_offset.to_le_bytes());
    bytes.extend(fwd_classes);
    bytes.extend(fwd_rows.into_iter().flat_map(u16::to_le_bytes));
    if let Some(nfa_blob) = nfa_bytes {
        bytes.extend(nfa_blob);
    }
    Ok(bytes)
}

fn compile_nfa(nfa: &thompson::NFA) -> Result<Vec<u8>> {
    let states = nfa.states();
    let state_count = u16::try_from(states.len()).context("Regex NFA state count overflow")?;
    let start_state =
        u16::try_from(nfa.start_anchored().as_usize()).context("Regex NFA start overflow")?;
    let mut state_offsets = Vec::with_capacity(states.len());
    let mut body = Vec::new();
    for state in states {
        let offset = u16::try_from(body.len()).context("Regex NFA bytecode size overflow")?;
        state_offsets.push(offset);
        match *state {
            State::Fail => body.push(0),
            State::ByteRange { ref trans } => {
                body.push(1);
                body.push(trans.start);
                body.push(trans.end);
                body.extend(u16::try_from(trans.next.as_usize())?.to_le_bytes());
            }
            State::Sparse(ref sparse) => {
                body.push(2);
                let count = u16::try_from(sparse.transitions.len())?;
                body.extend(count.to_le_bytes());
                for t in &sparse.transitions {
                    body.push(t.start);
                    body.push(t.end);
                    body.extend(u16::try_from(t.next.as_usize())?.to_le_bytes());
                }
            }
            State::Dense(ref dense) => {
                let mut ranges = Vec::<(u8, u8, u16)>::new();
                for (byte, &next_id) in dense.transitions.iter().enumerate() {
                    if matches!(states[next_id.as_usize()], State::Fail) {
                        continue;
                    }
                    let b = byte as u8;
                    let next = u16::try_from(next_id.as_usize())?;
                    if let Some(last) = ranges.last_mut()
                        && last.1.wrapping_add(1) == b
                        && last.2 == next
                    {
                        last.1 = b;
                    } else {
                        ranges.push((b, b, next));
                    }
                }
                body.push(2);
                let count = u16::try_from(ranges.len())?;
                body.extend(count.to_le_bytes());
                for (lo, hi, next) in ranges {
                    body.push(lo);
                    body.push(hi);
                    body.extend(next.to_le_bytes());
                }
            }
            State::Look { look, next } => {
                let code = match look {
                    Look::Start => 1u8,
                    Look::End => 2u8,
                    Look::StartLF => 3u8,
                    Look::EndLF => 4u8,
                    Look::WordAscii => 5u8,
                    Look::WordAsciiNegate => 6u8,
                    Look::WordStartAscii => 7u8,
                    Look::WordEndAscii => 8u8,
                    other => bail!("Unsupported regex look assertion: {other:?}"),
                };
                body.push(3);
                body.push(code);
                body.extend(u16::try_from(next.as_usize())?.to_le_bytes());
            }
            State::Union { ref alternates } => {
                body.push(4);
                let count = u16::try_from(alternates.len())?;
                body.extend(count.to_le_bytes());
                for &alt in alternates.iter() {
                    body.extend(u16::try_from(alt.as_usize())?.to_le_bytes());
                }
            }
            State::BinaryUnion { alt1, alt2 } => {
                body.push(4);
                body.extend(2u16.to_le_bytes());
                body.extend(u16::try_from(alt1.as_usize())?.to_le_bytes());
                body.extend(u16::try_from(alt2.as_usize())?.to_le_bytes());
            }
            State::Capture { next, slot, .. } => {
                body.push(5);
                body.push(u8::try_from(slot.as_usize()).unwrap_or(u8::MAX));
                body.extend(u16::try_from(next.as_usize())?.to_le_bytes());
            }
            State::Match { .. } => body.push(6),
        }
    }
    let mut out = Vec::with_capacity(4 + state_offsets.len() * 2 + body.len());
    out.extend(state_count.to_le_bytes());
    out.extend(start_state.to_le_bytes());
    for offset in state_offsets {
        out.extend(offset.to_le_bytes());
    }
    out.extend(body);
    Ok(out)
}
