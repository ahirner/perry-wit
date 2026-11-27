//! Bounded literal regexes compiled into immutable forward and reverse DFA tables.
use indexmap::IndexSet;
use perry_abi::layout::regex::{
    CAPTURE_PROGRAM, GLOBAL, GROUP_COUNT, LookCode, MAX_STATES, NAMES_OFFSET, NFA_HEADER, NfaOp,
    PROGRAM_HEADER, REVERSE_OFFSET,
};
use perry_wit_diagnostics::{Diagnostic, Result, bail_diag, codes, diag, ensure_diag};
use regex_automata::{
    Anchored, MatchKind,
    dfa::{Automaton, StartKind, dense},
    nfa::thompson::{self, State},
    util::{look::Look, primitives::StateID, start},
};

pub(super) fn compile(literal: &perry_mir::RegexLiteral) -> Result<Vec<u8>> {
    let parsed = literal.parsed()?;
    let is_global = literal.global();
    let group_count = table_len::<u16>(1 + parsed.captures.count as usize)?;
    let config = thompson::Config::new().nfa_size_limit(Some(1024 * 1024));
    let nfa = |config| {
        thompson::Compiler::new()
            .configure(config)
            .build_from_hir(&parsed.hir)
            .map_err(|err| perry_mir::RegexLiteral::build_error(&err))
    };
    let tables = dense::Config::new()
        .minimize(true)
        .dfa_size_limit(Some(4 * 1024 * 1024))
        .determinize_size_limit(Some(8 * 1024 * 1024));
    let mut bytes = vec![0; PROGRAM_HEADER];
    bytes[GROUP_COUNT..GROUP_COUNT + 2].copy_from_slice(&group_count.to_le_bytes());
    let forward = nfa(config.clone())?;
    let reverse = nfa(config
        .reverse(true)
        .which_captures(thompson::WhichCaptures::None))?;
    for (automaton, anchored, start_kind, match_kind) in [
        (
            &forward,
            Anchored::No,
            StartKind::Unanchored,
            MatchKind::LeftmostFirst,
        ),
        (&reverse, Anchored::Yes, StartKind::Anchored, MatchKind::All),
    ] {
        if anchored == Anchored::Yes {
            let offset = table_len::<u32>(bytes.len())? | if is_global { GLOBAL } else { 0 };
            bytes[REVERSE_OFFSET..REVERSE_OFFSET + 4].copy_from_slice(&offset.to_le_bytes());
        }
        let dfa = dense::Builder::new()
            .configure(tables.clone().start_kind(start_kind).match_kind(match_kind))
            .build_from_nfa(automaton)
            .map_err(|err| perry_mir::RegexLiteral::build_error(&err))?;
        write_dfa(&mut bytes, &dfa, anchored)?;
    }
    if group_count > 1 {
        let offset = table_len::<u32>(bytes.len())?;
        bytes[CAPTURE_PROGRAM..CAPTURE_PROGRAM + 4].copy_from_slice(&offset.to_le_bytes());
        write_nfa(&mut bytes, &forward)?;
    }
    let names_offset = table_len::<u32>(bytes.len())?;
    bytes[NAMES_OFFSET..NAMES_OFFSET + 4].copy_from_slice(&names_offset.to_le_bytes());
    for (slot, name) in &parsed.captures.names {
        bytes.extend(table_len::<u16>(*slot)?.to_le_bytes());
        let name_len = u16::try_from(name.len()).map_err(|_| {
            Diagnostic::new(
                codes::UNSUPPORTED_REGEX,
                "Regex capture group name is too long",
            )
        })?;
        bytes.extend(name_len.to_le_bytes());
        bytes.extend(name.as_bytes());
    }
    bytes.extend(0u16.to_le_bytes());
    Ok(bytes)
}

/// Append rows in discovery order. Transitions are 16-bit byte offsets from the first row.
fn write_dfa(bytes: &mut Vec<u8>, dfa: &dense::DFA<Vec<u32>>, anchored: Anchored) -> Result<()> {
    let classes = dfa.byte_classes();
    let end_column = table_len::<u16>(classes.alphabet_len() * 2)?;
    bytes.extend(end_column.to_le_bytes());
    let stride = usize::from(end_column) + 2;
    let mut states = IndexSet::new();
    let offset = |states: &mut IndexSet<StateID>, state| {
        u16::try_from(states.insert_full(state).0 * stride).map_err(|_| {
            diag!(
                codes::UNSUPPORTED_REGEX,
                "Regex state table exceeds size limit"
            )
        })
    };
    for adjacent in [None, Some(b'a'), Some(b' ')] {
        let start = dfa
            .start_state(
                &start::Config::new()
                    .anchored(anchored)
                    .look_behind(adjacent),
            )
            .map_err(|err| {
                diag!(
                    codes::UNSUPPORTED_REGEX,
                    "Regex requires an unsupported runtime fallback"
                )
                .with_source_notes(&err)
            })?;
        bytes.extend(offset(&mut states, start)?.to_le_bytes());
    }
    bytes.extend((0..=u8::MAX).map(|byte| classes.get(byte)));
    let mut index = 0;
    while let Some(&state) = states.get_index(index) {
        index += 1;
        ensure_diag!(
            !dfa.is_quit_state(state),
            codes::UNSUPPORTED_REGEX,
            "Regex requires an unsupported runtime fallback"
        );
        let flags =
            u16::from(dfa.is_match_state(state)) | (u16::from(dfa.is_dead_state(state)) << 1);
        bytes.extend(flags.to_le_bytes());
        for unit in classes.representatives(..) {
            let next = match unit.as_u8() {
                Some(byte) => dfa.next_state(state, byte),
                None => dfa.next_eoi_state(state),
            };
            bytes.extend(offset(&mut states, next)?.to_le_bytes());
        }
    }
    Ok(())
}

fn table_len<T: TryFrom<usize>>(value: usize) -> Result<T> {
    T::try_from(value)
        .map_err(|_| Diagnostic::new(codes::UNSUPPORTED_REGEX, "Regex table exceeds size limit"))
}

fn write_nfa(body: &mut Vec<u8>, nfa: &thompson::NFA) -> Result<()> {
    let states = nfa.states();
    ensure_diag!(
        states.len() <= MAX_STATES,
        codes::UNSUPPORTED_REGEX,
        "Regex capture program exceeds {MAX_STATES} states"
    );
    let header = body.len();
    body.extend(table_len::<u16>(states.len())?.to_le_bytes());
    body.extend(table_len::<u16>(nfa.start_anchored().as_usize())?.to_le_bytes());
    let instructions = header + NFA_HEADER + states.len() * 2;
    body.resize(instructions, 0);
    for (id, state) in states.iter().enumerate() {
        let offset = u16::try_from(body.len() - instructions).map_err(|_| {
            diag!(
                codes::UNSUPPORTED_REGEX,
                "Regex capture program exceeds bytecode size limit"
            )
        })?;
        let slot = header + NFA_HEADER + id * 2;
        body[slot..slot + 2].copy_from_slice(&offset.to_le_bytes());
        match *state {
            State::Fail => body.push(NfaOp::Fail as u8),
            State::ByteRange { trans } => write_ranges(body, core::iter::once(trans))?,
            State::Sparse(ref sparse) => write_ranges(body, sparse.transitions.iter().copied())?,
            State::Dense(ref dense) => {
                let mut start = 0;
                let ranges = dense.transitions.chunk_by(|a, b| a == b).filter_map(|run| {
                    let next = run[0];
                    let lo = start;
                    start += run.len();
                    (next != StateID::ZERO).then_some(thompson::Transition {
                        start: lo as u8,
                        end: (start - 1) as u8,
                        next,
                    })
                });
                write_ranges(body, ranges)?;
            }
            State::Look { look, next } => {
                let code = match look {
                    Look::Start => LookCode::Start,
                    Look::End => LookCode::End,
                    Look::StartLF => LookCode::StartLine,
                    Look::EndLF => LookCode::EndLine,
                    Look::WordAscii => LookCode::WordBoundary,
                    Look::WordAsciiNegate => LookCode::NotWordBoundary,
                    Look::WordStartAscii => LookCode::WordStart,
                    Look::WordEndAscii => LookCode::WordEnd,
                    other => bail_diag!(
                        codes::UNSUPPORTED_REGEX,
                        "Unsupported regex look assertion: {other:?}"
                    ),
                };
                body.push(NfaOp::Look as u8);
                body.push(code as u8);
                body.extend(table_len::<u16>(next.as_usize())?.to_le_bytes());
            }
            State::Union { ref alternates } => {
                body.push(NfaOp::Union as u8);
                for &alt in alternates.iter() {
                    body.extend(table_len::<u16>(alt.as_usize())?.to_le_bytes());
                }
            }
            State::BinaryUnion { alt1, alt2 } => {
                body.push(NfaOp::Union as u8);
                body.extend(table_len::<u16>(alt1.as_usize())?.to_le_bytes());
                body.extend(table_len::<u16>(alt2.as_usize())?.to_le_bytes());
            }
            State::Capture { next, slot, .. } => {
                body.push(NfaOp::Capture as u8);
                body.push(table_len::<u8>(slot.as_usize())?);
                body.extend(table_len::<u16>(next.as_usize())?.to_le_bytes());
            }
            State::Match { .. } => body.push(NfaOp::Match as u8),
        }
    }
    Ok(())
}

fn write_ranges(
    bytes: &mut Vec<u8>,
    ranges: impl Iterator<Item = thompson::Transition>,
) -> Result<()> {
    bytes.push(NfaOp::Ranges as u8);
    for range in ranges {
        bytes.extend([range.start, range.end]);
        bytes.extend(table_len::<u16>(range.next.as_usize())?.to_le_bytes());
    }
    Ok(())
}
