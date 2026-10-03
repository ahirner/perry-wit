//! Literal regex search compiled to immutable reverse DFA tables.

mod syntax;

use std::collections::{BTreeMap, VecDeque};

use anyhow::{Context, Result, ensure};
use perry_hir::ir::{Expr, Module as HirModule};
use regex_automata::{
    Input, MatchKind,
    dfa::{Automaton, dense},
    nfa::thompson,
};
use waffle::{Func, Memory, MemorySegment, Module};

use super::visit;

pub(crate) type Pattern = (String, String);

pub(crate) struct RegexSearch {
    pub(crate) programs: BTreeMap<Pattern, u32>,
    pub(crate) function: Func,
}

pub(crate) fn compile_literals(hir: &HirModule) -> Result<BTreeMap<Pattern, Vec<u8>>> {
    let mut programs = BTreeMap::new();
    let mut visit = |expr: &Expr| {
        if let Expr::RegExp { pattern, flags } = expr {
            programs
                .entry((pattern.clone(), flags.clone()))
                .or_insert_with(|| compile(pattern, flags));
        }
    };
    for function in &hir.functions {
        visit::visit_function_expressions(function, &mut visit);
    }
    programs
        .into_iter()
        .map(|(key, bytes)| Ok((key, bytes?)))
        .collect()
}

pub(crate) fn emit_tables(
    module: &mut Module<'static>,
    memory: Memory,
    tables: BTreeMap<Pattern, Vec<u8>>,
    mut offset: u32,
) -> Result<(BTreeMap<Pattern, u32>, u32)> {
    let mut programs = BTreeMap::new();
    for (pattern, bytes) in tables {
        let size = u32::try_from(bytes.len())?;
        programs.insert(pattern, offset);
        module.memories[memory].segments.push(MemorySegment {
            offset: offset as usize,
            data: bytes,
        });
        offset = offset
            .checked_add(size)
            .context("Regex data exceeds wasm32 memory")?;
    }
    module.memories[memory].initial_pages = module.memories[memory]
        .initial_pages
        .max(offset.div_ceil(65_536) as usize);
    Ok((programs, offset))
}

pub(crate) fn emit_runtime(
    module: &mut Module<'static>,
    memory: Memory,
    programs: BTreeMap<Pattern, u32>,
) -> Result<Option<RegexSearch>> {
    if programs.is_empty() {
        return Ok(None);
    }
    let functions = super::runtime::emit_functions(
        module,
        memory,
        include_str!("regex/search.wat"),
        &BTreeMap::new(),
    )?;
    let function = functions["search"];
    Ok(Some(RegexSearch { programs, function }))
}

fn compile(pattern: &str, flags: &str) -> Result<Vec<u8>> {
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
    let class_count = usize::from(*classes.iter().max().unwrap()) + 1;
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
