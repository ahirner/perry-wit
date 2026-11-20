//! Bounded literal regexes compiled into immutable reverse and forward DFA tables.
mod syntax;
use perry_abi::layout::regex::{
    FORWARD_ROWS, GLOBAL, LookCode, MAX_CAPTURES, MAX_STATES, NAMES_OFFSET, NFA_HEADER, NfaOp,
    REVERSE_ROWS,
};
use perry_wit_diagnostics::{Diagnostic, Result, bail_diag, codes, diag, ensure_diag};
use regex_automata::{
    Anchored, Input, MatchKind, PatternID,
    dfa::{Automaton, StartKind, dense},
    nfa::thompson::{self, State},
    util::{look::Look, primitives::StateID},
};
use std::collections::{BTreeMap, VecDeque};

fn regex_build_error(err: &(dyn std::error::Error + 'static)) -> perry_wit_diagnostics::Diagnostic {
    diag!(
        codes::UNSUPPORTED_REGEX,
        "Unsupported or oversized literal regex"
    )
    .with_source_notes(err)
}

pub(super) fn compile(pattern: &str, flags: &str) -> Result<Vec<u8>> {
    let is_global = flags.contains('g');
    let pattern = syntax::normalize(pattern, flags)?;
    let nfa = thompson::Compiler::new()
        .configure(thompson::Config::new().nfa_size_limit(Some(1024 * 1024)))
        .build(&pattern)
        .map_err(|err| regex_build_error(&err))?;
    // Group 0 is the whole match; the matcher tracks `MAX_CAPTURES` explicit groups after it.
    let groups = nfa.group_info().all_group_len();
    ensure_diag!(
        groups <= 1 + MAX_CAPTURES,
        codes::UNSUPPORTED_REGEX,
        "Regex has {} capture groups; at most {MAX_CAPTURES} are supported",
        groups - 1
    );
    let group_count = u16::try_from(groups)
        .map_err(|_| Diagnostic::internal("Regex capture group count overflow"))?;

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
        .map_err(|err| regex_build_error(&err))?;
    let unsupported_start = |err: regex_automata::MatchError| {
        diag!(
            codes::UNSUPPORTED_REGEX,
            "Regex requires an unsupported runtime fallback"
        )
        .with_source_notes(&err)
    };
    let start = dfa
        .start_state_reverse(&Input::new(b""))
        .map_err(unsupported_start)?;
    let reverse = DfaTable::new(&dfa, [start], false)?;

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
        .map_err(|err| regex_build_error(&err))?;
    let start_bof = fwd_dfa
        .start_state_forward(&Input::new(b"").anchored(Anchored::Yes))
        .map_err(unsupported_start)?;
    let start_word = fwd_dfa
        .start_state_forward(&Input::new(&b"a"[..]).range(1..1).anchored(Anchored::Yes))
        .map_err(unsupported_start)?;
    let start_nonword = fwd_dfa
        .start_state_forward(&Input::new(&b" "[..]).range(1..1).anchored(Anchored::Yes))
        .map_err(unsupported_start)?;
    let forward = DfaTable::new(&fwd_dfa, [start_bof, start_word, start_nonword], true)?;

    let rev_bytes_len = reverse.rows.len() * 2;
    let rev_padding = (4 - (rev_bytes_len & 3)) & 3;
    let fwd_offset = table_len::<u32>(REVERSE_ROWS + rev_bytes_len + rev_padding)?;
    let fwd_header = fwd_offset | if is_global { GLOBAL } else { 0 };
    let fwd_bytes_len = forward.rows.len() * 2;
    let nfa_bytes = if group_count > 1 {
        Some(compile_nfa(&nfa)?)
    } else {
        None
    };
    let nfa_offset = if nfa_bytes.is_some() {
        fwd_offset
            .checked_add(table_len::<u32>(FORWARD_ROWS + fwd_bytes_len)?)
            .ok_or_else(|| {
                Diagnostic::new(codes::UNSUPPORTED_REGEX, "Regex table exceeds size limit")
            })?
    } else {
        0
    };

    let mut bytes = Vec::new();
    bytes.extend(reverse.end_column.to_le_bytes());
    bytes.extend(group_count.to_le_bytes());
    bytes.extend(fwd_header.to_le_bytes());
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(reverse.classes);
    bytes.extend(reverse.rows.into_iter().flat_map(u16::to_le_bytes));
    bytes.resize(bytes.len() + rev_padding, 0);

    bytes.extend(forward.end_column.to_le_bytes());
    bytes.extend(forward.starts.into_iter().flat_map(u16::to_le_bytes));
    bytes.extend(nfa_offset.to_le_bytes());
    bytes.extend(forward.classes);
    bytes.extend(forward.rows.into_iter().flat_map(u16::to_le_bytes));
    if let Some(nfa_blob) = nfa_bytes {
        bytes.extend(nfa_blob);
    }
    let names_offset = table_len::<u32>(bytes.len())?;
    bytes[NAMES_OFFSET..NAMES_OFFSET + 4].copy_from_slice(&names_offset.to_le_bytes());
    for (index, name) in nfa.group_info().pattern_names(PatternID::ZERO).enumerate() {
        if let Some(name) = name {
            bytes.extend(table_len::<u16>(index)?.to_le_bytes());
            let name_len = u16::try_from(name.len()).map_err(|_| {
                Diagnostic::new(
                    codes::UNSUPPORTED_REGEX,
                    "Regex capture group name is too long",
                )
            })?;
            bytes.extend(name_len.to_le_bytes());
            bytes.extend(name.as_bytes());
        }
    }
    bytes.extend(0u16.to_le_bytes());
    Ok(bytes)
}

struct DfaTable<const N: usize> {
    classes: [u8; 256],
    rows: Vec<u16>,
    end_column: u16,
    starts: [u16; N],
}

impl<const N: usize> DfaTable<N> {
    fn new(dfa: &dense::DFA<Vec<u32>>, starts: [StateID; N], track_dead: bool) -> Result<Self> {
        let classes = std::array::from_fn(|byte| dfa.byte_classes().get(byte as u8));
        let class_count = dfa.byte_classes().alphabet_len() - 1;
        let mut representatives = vec![0u8; class_count];
        for (byte, &class) in classes.iter().enumerate() {
            representatives[usize::from(class)] = byte as u8;
        }
        let stride = u16::try_from((class_count + 2) * 2)
            .map_err(|_| Diagnostic::internal("Regex state stride overflow"))?;
        let mut ids = BTreeMap::new();
        let mut pending = VecDeque::new();
        let mut offsets = [0; N];
        for (index, start) in starts.into_iter().enumerate() {
            offsets[index] = state_offset(&mut ids, &mut pending, start, stride)?;
        }
        let mut rows = Vec::new();
        while let Some(state) = pending.pop_front() {
            ensure_diag!(
                !dfa.is_quit_state(state),
                codes::UNSUPPORTED_REGEX,
                "Regex requires an unsupported runtime fallback"
            );
            rows.push(
                u16::from(dfa.is_match_state(state))
                    | (u16::from(track_dead && dfa.is_dead_state(state)) << 1),
            );
            for next in representatives
                .iter()
                .map(|&byte| dfa.next_state(state, byte))
                .chain([dfa.next_eoi_state(state)])
            {
                rows.push(state_offset(&mut ids, &mut pending, next, stride)?);
            }
        }
        Ok(Self {
            classes,
            rows,
            end_column: table_len::<u16>((class_count + 1) * 2)?,
            starts: offsets,
        })
    }
}

fn state_offset(
    ids: &mut BTreeMap<StateID, u16>,
    pending: &mut VecDeque<StateID>,
    state: StateID,
    stride: u16,
) -> Result<u16> {
    const LIMIT: &str = "Regex state table exceeds size limit";
    let next_id =
        u16::try_from(ids.len()).map_err(|_| Diagnostic::new(codes::UNSUPPORTED_REGEX, LIMIT))?;
    let id = *ids.entry(state).or_insert_with(|| {
        pending.push_back(state);
        next_id
    });
    id.checked_mul(stride)
        .ok_or_else(|| Diagnostic::new(codes::UNSUPPORTED_REGEX, LIMIT))
}

fn table_len<T: TryFrom<usize>>(value: usize) -> Result<T> {
    T::try_from(value)
        .map_err(|_| Diagnostic::new(codes::UNSUPPORTED_REGEX, "Regex table exceeds size limit"))
}

fn compile_nfa(nfa: &thompson::NFA) -> Result<Vec<u8>> {
    let states = nfa.states();
    ensure_diag!(
        states.len() <= MAX_STATES,
        codes::UNSUPPORTED_REGEX,
        "Regex capture program exceeds {MAX_STATES} states"
    );
    let state_count = u16::try_from(states.len())
        .map_err(|_| Diagnostic::internal("Regex NFA state count overflow"))?;
    let start_state = u16::try_from(nfa.start_anchored().as_usize())
        .map_err(|_| Diagnostic::internal("Regex NFA start overflow"))?;
    let mut state_offsets = Vec::with_capacity(states.len());
    let mut body = Vec::new();
    for state in states {
        let offset = u16::try_from(body.len()).map_err(|_| {
            Diagnostic::new(
                codes::UNSUPPORTED_REGEX,
                "Regex capture program exceeds bytecode size limit",
            )
        })?;
        state_offsets.push(offset);
        match *state {
            State::Fail => body.push(NfaOp::Fail as u8),
            State::ByteRange { ref trans } => {
                body.push(NfaOp::Range as u8);
                body.push(trans.start);
                body.push(trans.end);
                body.extend(table_len::<u16>(trans.next.as_usize())?.to_le_bytes());
            }
            State::Sparse(ref sparse) => {
                body.push(NfaOp::Sparse as u8);
                let count = table_len::<u16>(sparse.transitions.len())?;
                body.extend(count.to_le_bytes());
                for t in &sparse.transitions {
                    body.push(t.start);
                    body.push(t.end);
                    body.extend(table_len::<u16>(t.next.as_usize())?.to_le_bytes());
                }
            }
            State::Dense(ref dense) => {
                let mut ranges = Vec::<(u8, u8, u16)>::new();
                for (byte, &next_id) in dense.transitions.iter().enumerate() {
                    if matches!(states[next_id.as_usize()], State::Fail) {
                        continue;
                    }
                    let b = byte as u8;
                    let next = table_len::<u16>(next_id.as_usize())?;
                    if let Some(last) = ranges.last_mut()
                        && last.1.wrapping_add(1) == b
                        && last.2 == next
                    {
                        last.1 = b;
                    } else {
                        ranges.push((b, b, next));
                    }
                }
                body.push(NfaOp::Sparse as u8);
                let count = table_len::<u16>(ranges.len())?;
                body.extend(count.to_le_bytes());
                for (lo, hi, next) in ranges {
                    body.push(lo);
                    body.push(hi);
                    body.extend(next.to_le_bytes());
                }
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
                let count = table_len::<u16>(alternates.len())?;
                body.extend(count.to_le_bytes());
                for &alt in alternates.iter() {
                    body.extend(table_len::<u16>(alt.as_usize())?.to_le_bytes());
                }
            }
            State::BinaryUnion { alt1, alt2 } => {
                body.push(NfaOp::Union as u8);
                body.extend(2u16.to_le_bytes());
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
    let size = NFA_HEADER + state_offsets.len() * 2 + body.len();
    let mut out = Vec::with_capacity(size);
    out.extend(table_len::<u32>(size)?.to_le_bytes());
    out.extend(state_count.to_le_bytes());
    out.extend(start_state.to_le_bytes());
    for offset in state_offsets {
        out.extend(offset.to_le_bytes());
    }
    out.extend(body);
    Ok(out)
}
