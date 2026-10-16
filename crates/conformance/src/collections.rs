//! Orthogonal state-machine generator and reference model for live `Set` and `Map` iterators.
//!
//! Covers the product of:
//! - collection views (`Set.values()`, `Map.keys()`, `Map.values()`, `Map.entries()`),
//! - multiple concurrent iterator lifetimes (active loop, paused via `break`, exhausted, and
//!   overwritten/abandoned),
//! - structural mutations both between loops and inside active loop bodies (`add`/`set`,
//!   `delete`, delete + reinsert, `clear`, and batch deletes across the 32-tombstone compaction
//!   threshold),
//! - declaration-order permutation (`permuted`).

use crate::{
    execution::{Evidence, Observation, Outcome, check_equivalence, number},
    program::INPUTS,
    registry::{Case, Contract, Witness},
};
use anyhow::{Result, ensure};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

/// Number of iterator variable slots maintained concurrently by the generated state machine.
const NUM_ITER_SLOTS: usize = 3;

/// Which collection and iterator view the state machine exercises.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionKind {
    SetValues,
    SetEntries,
    MapKeys,
    MapValues,
    MapEntries,
}

impl CollectionKind {
    fn is_set(self) -> bool {
        matches!(self, Self::SetValues | Self::SetEntries)
    }
}

/// Mutation performed on the first step inside an active `for...of` or `forEach` body.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InLoopMutation {
    None,
    DeleteCurrent,
    DeleteAndReinsert,
    InsertOffset(i32),
    ClearAndInsert(i32),
}

/// A transition in the collection + iterator state machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CollectionAction {
    /// Allocate a fresh iterator from the collection into `slot` (abandoning the previous one).
    NewIter { slot: usize },
    /// Resume `slot` in a `for...of` loop for at most `steps` items (`1..=4`), or until
    /// exhaustion (`steps == 0` means drain to completion).
    Step {
        slot: usize,
        steps: usize,
        in_body: InLoopMutation,
    },
    /// Traverse the collection via `.forEach((v, k) => ...)` with an optional first-element
    /// mutation, while existing `for...of` iterators remain paused.
    ForEach { in_body: InLoopMutation },
    /// Insert or update a single key (`val_bias` in `0..=9`).
    Insert { key: i32, val_bias: i32 },
    /// Insert a contiguous batch of keys (`count` up to 48) so subsequent batch deletes cross
    /// the compaction threshold.
    InsertBatch { start_key: i32, count: usize },
    /// Delete a single key.
    Delete { key: i32 },
    /// Delete a contiguous batch of keys (`count` up to 40, triggering compaction when
    /// `dead >= 32 && dead >= live` or empty-storage reset when `live == 0`).
    DeleteBatch { start_key: i32, count: usize },
    /// Clear all entries while existing iterators remain live or paused.
    Clear,
    /// Run a throwaway loop that breaks after 1 item to churn abandoned cursors.
    ChurnAbandoned { count: usize },
}

/// Generated state-machine case over a live `Set` or `Map`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionCase {
    pub kind: CollectionKind,
    pub initial_count: usize,
    pub actions: Vec<CollectionAction>,
    pub permuted: bool,
}

#[derive(Clone, Debug)]
struct IterModel {
    pos: usize,
    exhausted: bool,
}

/// Trivial `Vec`-backed ECMAScript reference model for `Set` and `Map` live iteration.
#[derive(Clone, Debug)]
struct ReferenceCollection {
    kind: CollectionKind,
    entries: Vec<(i32, i32)>,
    iters: [IterModel; NUM_ITER_SLOTS],
    for_each_pos: Option<usize>,
}

impl ReferenceCollection {
    fn new(kind: CollectionKind, initial_count: usize, input_bias: i32) -> Self {
        let mut entries = Vec::with_capacity(initial_count);
        for i in 0..initial_count {
            let k = i as i32;
            let v = k * 100 + input_bias;
            entries.push((k, v));
        }
        Self {
            kind,
            entries,
            iters: std::array::from_fn(|_| IterModel {
                pos: 0,
                exhausted: false,
            }),
            for_each_pos: None,
        }
    }

    fn value_for(&self, key: i32, bias: i32) -> i32 {
        key * 100 + bias
    }

    fn insert(&mut self, key: i32, val: i32) {
        if let Some(existing) = self.entries.iter_mut().find(|(k, _)| *k == key) {
            if !self.kind.is_set() {
                existing.1 = val;
            }
        } else {
            self.entries.push((key, val));
        }
    }

    fn delete(&mut self, key: i32) {
        if let Some(idx) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(idx);
            for iter in &mut self.iters {
                if !iter.exhausted && iter.pos > idx {
                    iter.pos -= 1;
                }
            }
            if let Some(pos) = &mut self.for_each_pos
                && *pos > idx
            {
                *pos -= 1;
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        for iter in &mut self.iters {
            if !iter.exhausted {
                iter.pos = 0;
            }
        }
        if let Some(pos) = &mut self.for_each_pos {
            *pos = 0;
        }
    }

    fn new_iter(&mut self, slot: usize) {
        let slot = slot % NUM_ITER_SLOTS;
        self.iters[slot] = IterModel {
            pos: 0,
            exhausted: false,
        };
    }

    fn apply_in_body(&mut self, k: i32, in_body: InLoopMutation, input_bias: i32) {
        match in_body {
            InLoopMutation::None => {}
            InLoopMutation::DeleteCurrent => {
                self.delete(k);
            }
            InLoopMutation::DeleteAndReinsert => {
                self.delete(k);
                self.insert(k, self.value_for(k, input_bias + 1));
            }
            InLoopMutation::InsertOffset(off) => {
                let nk = k + off;
                self.insert(nk, self.value_for(nk, input_bias));
            }
            InLoopMutation::ClearAndInsert(off) => {
                let nk = k + off;
                self.clear();
                self.insert(nk, self.value_for(nk, input_bias));
            }
        }
    }

    fn step(
        &mut self,
        slot: usize,
        steps: usize,
        in_body: InLoopMutation,
        input_bias: i32,
        trace: &mut Vec<f64>,
    ) {
        let slot = slot % NUM_ITER_SLOTS;
        let mut seen = 0usize;
        loop {
            if self.iters[slot].exhausted || self.iters[slot].pos >= self.entries.len() {
                self.iters[slot].exhausted = true;
                break;
            }
            let (k, v) = self.entries[self.iters[slot].pos];
            self.iters[slot].pos += 1;
            let yielded = match self.kind {
                CollectionKind::SetValues | CollectionKind::MapKeys => k,
                CollectionKind::SetEntries => k + k,
                CollectionKind::MapValues => v,
                CollectionKind::MapEntries => k + v,
            };
            trace.push(f64::from(yielded));
            seen += 1;
            if seen == 1 {
                self.apply_in_body(k, in_body, input_bias);
            }
            if steps > 0 && seen >= steps {
                break;
            }
        }
        trace.push(f64::from(-(seen as i32) - 100));
    }

    fn for_each(&mut self, in_body: InLoopMutation, input_bias: i32, trace: &mut Vec<f64>) {
        self.for_each_pos = Some(0);
        let mut seen = 0usize;
        loop {
            let pos = self.for_each_pos.unwrap_or(0);
            if pos >= self.entries.len() {
                break;
            }
            let (k, v) = self.entries[pos];
            self.for_each_pos = Some(pos + 1);
            let yielded = if self.kind.is_set() { k + k } else { k + v };
            trace.push(f64::from(yielded));
            seen += 1;
            if seen == 1 {
                self.apply_in_body(k, in_body, input_bias);
            }
        }
        self.for_each_pos = None;
        trace.push(f64::from(-(seen as i32) - 200));
    }

    fn apply(&mut self, action: CollectionAction, input_bias: i32, trace: &mut Vec<f64>) {
        match action {
            CollectionAction::NewIter { slot } => self.new_iter(slot),
            CollectionAction::Step {
                slot,
                steps,
                in_body,
            } => self.step(slot, steps, in_body, input_bias, trace),
            CollectionAction::ForEach { in_body } => self.for_each(in_body, input_bias, trace),
            CollectionAction::Insert { key, val_bias } => {
                let v = self.value_for(key, input_bias + val_bias.rem_euclid(10));
                self.insert(key, v);
            }
            CollectionAction::InsertBatch { start_key, count } => {
                for i in 0..count {
                    let k = start_key + i as i32;
                    let v = self.value_for(k, input_bias);
                    self.insert(k, v);
                }
            }
            CollectionAction::Delete { key } => self.delete(key),
            CollectionAction::DeleteBatch { start_key, count } => {
                for i in 0..count {
                    self.delete(start_key + i as i32);
                }
            }
            CollectionAction::Clear => self.clear(),
            CollectionAction::ChurnAbandoned { .. } => {
                // Reading a temporary iterator and breaking does not mutate the collection or
                // the retained iterator slots.
            }
        }
    }
}

impl CollectionCase {
    /// Return a copy of this case with declaration order reversed.
    pub fn with_permuted(&self, permuted: bool) -> Self {
        let mut copy = self.clone();
        copy.permuted = permuted;
        copy
    }

    /// Evaluate the exact expected observation for input `x` using the Rust reference model.
    pub(crate) fn evaluate(&self, x: f64) -> Observation {
        let input_bias = if x.is_finite() {
            x.trunc().clamp(-50.0, 50.0) as i32
        } else {
            0
        };
        let mut model = ReferenceCollection::new(self.kind, self.initial_count, input_bias);
        let mut trace = Vec::new();
        for &action in &self.actions {
            model.apply(action, input_bias, &mut trace);
        }
        // Final drain of all retained iterator slots so every paused iterator's remaining tail is
        // verified.
        for slot in 0..NUM_ITER_SLOTS {
            model.step(slot, 0, InLoopMutation::None, input_bias, &mut trace);
        }
        let size = model.entries.len() as f64;
        let checksum: f64 = trace
            .iter()
            .enumerate()
            .fold(size, |acc, (idx, &v)| acc + v * ((idx % 7) as f64 + 1.0));
        Observation {
            value: number(checksum),
            trace: trace.into_iter().map(number).collect(),
        }
    }

    /// Expected observations across `INPUTS`.
    pub(crate) fn expected_observations(&self) -> Vec<Observation> {
        INPUTS.into_iter().map(|x| self.evaluate(x)).collect()
    }

    /// Emit a standalone TypeScript module exporting `run(x: number): Observation`.
    pub fn source(&self) -> String {
        let mut out = String::new();
        out.push_str("export interface Observation {\n  value: number;\n  trace: number[];\n}\n\n");
        let helpers = [
            "function normalizeBias(x: number): number {\n  if (!Number.isFinite(x)) return 0;\n  const t = Math.trunc(x);\n  return t < -50 ? -50 : (t > 50 ? 50 : t);\n}\n",
            "function valueFor(k: number, bias: number): number {\n  return k * 100 + bias;\n}\n",
            "function finalizeChecksum(size: number, trace: number[]): number {\n  let acc = size;\n  for (let i = 0; i < trace.length; i += 1) {\n    acc += trace[i] * ((i % 7) + 1);\n  }\n  return acc;\n}\n",
        ];
        if self.permuted {
            for h in helpers.iter().rev() {
                out.push_str(h);
                out.push('\n');
            }
        } else {
            for h in &helpers {
                out.push_str(h);
                out.push('\n');
            }
        }

        out.push_str("export function run(x: number): Observation {\n");
        out.push_str(&self.emit_body("  "));
        out.push_str("}\n");
        out
    }

    fn emit_body(&self, indent: &str) -> String {
        let mut out = String::new();
        let is_set = self.kind.is_set();
        let view_call = match self.kind {
            CollectionKind::SetValues | CollectionKind::MapValues => "coll.values()",
            CollectionKind::MapKeys => "coll.keys()",
            CollectionKind::SetEntries | CollectionKind::MapEntries => "coll.entries()",
        };

        let decls = if is_set {
            vec![
                format!("{indent}const bias = normalizeBias(x);\n"),
                format!("{indent}const trace: number[] = [];\n"),
                format!("{indent}const coll = new Set<number>();\n"),
            ]
        } else {
            vec![
                format!("{indent}const bias = normalizeBias(x);\n"),
                format!("{indent}const trace: number[] = [];\n"),
                format!("{indent}const coll = new Map<number, number>();\n"),
            ]
        };
        if self.permuted {
            for d in decls.iter().rev() {
                out.push_str(d);
            }
        } else {
            for d in &decls {
                out.push_str(d);
            }
        }

        out.push_str(&format!(
            "{indent}for (let i = 0; i < {}; i += 1) {{\n",
            self.initial_count
        ));
        if is_set {
            out.push_str(&format!("{indent}  coll.add(i);\n"));
        } else {
            out.push_str(&format!("{indent}  coll.set(i, valueFor(i, bias));\n"));
        }
        out.push_str(&format!("{indent}}}\n"));

        let iter_decls: Vec<String> = (0..NUM_ITER_SLOTS)
            .map(|slot| format!("{indent}let it{slot} = {view_call};\n"))
            .collect();
        if self.permuted {
            for d in iter_decls.iter().rev() {
                out.push_str(d);
            }
        } else {
            for d in &iter_decls {
                out.push_str(d);
            }
        }

        for (step_idx, &action) in self.actions.iter().enumerate() {
            self.emit_action(&mut out, indent, step_idx, action, is_set, view_call);
        }

        for slot in 0..NUM_ITER_SLOTS {
            self.emit_step(
                &mut out,
                indent,
                self.actions.len() + slot,
                slot,
                0,
                InLoopMutation::None,
            );
        }

        out.push_str(&format!(
            "{indent}return {{ value: finalizeChecksum(coll.size, trace), trace }};\n"
        ));
        out
    }

    fn emit_in_body_mutation(&self, out: &mut String, indent: &str, in_body: InLoopMutation) {
        let is_set = self.kind.is_set();
        match in_body {
            InLoopMutation::None => {}
            InLoopMutation::DeleteCurrent => {
                out.push_str(&format!("{indent}coll.delete(k);\n"));
            }
            InLoopMutation::DeleteAndReinsert => {
                out.push_str(&format!("{indent}coll.delete(k);\n"));
                if is_set {
                    out.push_str(&format!("{indent}coll.add(k);\n"));
                } else {
                    out.push_str(&format!(
                        "{indent}coll.set(k, valueFor(k, bias + 1));\n"
                    ));
                }
            }
            InLoopMutation::InsertOffset(off) => {
                out.push_str(&format!("{indent}const nk = k + ({off});\n"));
                if is_set {
                    out.push_str(&format!("{indent}coll.add(nk);\n"));
                } else {
                    out.push_str(&format!("{indent}coll.set(nk, valueFor(nk, bias));\n"));
                }
            }
            InLoopMutation::ClearAndInsert(off) => {
                out.push_str(&format!("{indent}const nk = k + ({off});\n"));
                out.push_str(&format!("{indent}coll.clear();\n"));
                if is_set {
                    out.push_str(&format!("{indent}coll.add(nk);\n"));
                } else {
                    out.push_str(&format!("{indent}coll.set(nk, valueFor(nk, bias));\n"));
                }
            }
        }
    }

    fn emit_action(
        &self,
        out: &mut String,
        indent: &str,
        step_idx: usize,
        action: CollectionAction,
        is_set: bool,
        view_call: &str,
    ) {
        match action {
            CollectionAction::NewIter { slot } => {
                let slot = slot % NUM_ITER_SLOTS;
                out.push_str(&format!("{indent}it{slot} = {view_call};\n"));
            }
            CollectionAction::Step {
                slot,
                steps,
                in_body,
            } => {
                self.emit_step(out, indent, step_idx, slot % NUM_ITER_SLOTS, steps, in_body);
            }
            CollectionAction::ForEach { in_body } => {
                let seen_var = format!("fe_seen_{step_idx}");
                out.push_str(&format!("{indent}let {seen_var} = 0;\n"));
                out.push_str(&format!(
                    "{indent}coll.forEach((v, k) => {{\n{indent}  trace.push(k + v);\n{indent}  {seen_var} += 1;\n"
                ));
                if !matches!(in_body, InLoopMutation::None) {
                    out.push_str(&format!("{indent}  if ({seen_var} === 1) {{\n"));
                    self.emit_in_body_mutation(out, &format!("{indent}    "), in_body);
                    out.push_str(&format!("{indent}  }}\n"));
                }
                out.push_str(&format!("{indent}}});\n"));
                out.push_str(&format!("{indent}trace.push(-{seen_var} - 200);\n"));
            }
            CollectionAction::Insert { key, val_bias } => {
                let vb = val_bias.rem_euclid(10);
                if is_set {
                    out.push_str(&format!("{indent}coll.add({key});\n"));
                } else {
                    out.push_str(&format!(
                        "{indent}coll.set({key}, valueFor({key}, bias + {vb}));\n"
                    ));
                }
            }
            CollectionAction::InsertBatch { start_key, count } => {
                out.push_str(&format!(
                    "{indent}for (let b = 0; b < {count}; b += 1) {{\n{indent}  const k = ({start_key}) + b;\n"
                ));
                if is_set {
                    out.push_str(&format!("{indent}  coll.add(k);\n"));
                } else {
                    out.push_str(&format!("{indent}  coll.set(k, valueFor(k, bias));\n"));
                }
                out.push_str(&format!("{indent}}}\n"));
            }
            CollectionAction::Delete { key } => {
                out.push_str(&format!("{indent}coll.delete({key});\n"));
            }
            CollectionAction::DeleteBatch { start_key, count } => {
                out.push_str(&format!(
                    "{indent}for (let b = 0; b < {count}; b += 1) {{\n{indent}  coll.delete(({start_key}) + b);\n{indent}}}\n"
                ));
            }
            CollectionAction::Clear => {
                out.push_str(&format!("{indent}coll.clear();\n"));
            }
            CollectionAction::ChurnAbandoned { count } => {
                let loop_head = match self.kind {
                    CollectionKind::SetEntries | CollectionKind::MapEntries => {
                        "for (const [_k, _v] of tmpIt)"
                    }
                    _ => "for (const _v of tmpIt)",
                };
                let temp_churn = if is_set {
                    "for (let d = 0; d < 64; d += 1) { coll.add(10000 + d); coll.delete(10000 + d); }"
                } else {
                    "for (let d = 0; d < 64; d += 1) { coll.set(10000 + d, d); coll.delete(10000 + d); }"
                };
                out.push_str(&format!(
                    "{indent}for (let c = 0; c < {count}; c += 1) {{\n{indent}  {temp_churn}\n{indent}  const tmpIt = {view_call};\n{indent}  {loop_head} {{\n{indent}    break;\n{indent}  }}\n{indent}}}\n"
                ));
            }
        }
    }

    fn emit_step(
        &self,
        out: &mut String,
        indent: &str,
        step_idx: usize,
        slot: usize,
        steps: usize,
        in_body: InLoopMutation,
    ) {
        let seen_var = format!("seen_{step_idx}");
        out.push_str(&format!("{indent}let {seen_var} = 0;\n"));
        match self.kind {
            CollectionKind::SetEntries | CollectionKind::MapEntries => {
                out.push_str(&format!(
                    "{indent}for (const [k, v] of it{slot}) {{\n{indent}  trace.push(k + v);\n{indent}  {seen_var} += 1;\n"
                ));
            }
            CollectionKind::MapValues => {
                out.push_str(&format!(
                    "{indent}for (const v of it{slot}) {{\n{indent}  const k = Math.floor((v - bias) / 100);\n{indent}  trace.push(v);\n{indent}  {seen_var} += 1;\n"
                ));
            }
            CollectionKind::SetValues | CollectionKind::MapKeys => {
                out.push_str(&format!(
                    "{indent}for (const k of it{slot}) {{\n{indent}  trace.push(k);\n{indent}  {seen_var} += 1;\n"
                ));
            }
        }

        if !matches!(in_body, InLoopMutation::None) {
            out.push_str(&format!("{indent}  if ({seen_var} === 1) {{\n"));
            self.emit_in_body_mutation(out, &format!("{indent}    "), in_body);
            out.push_str(&format!("{indent}  }}\n"));
        }

        if steps > 0 {
            out.push_str(&format!(
                "{indent}  if ({seen_var} >= {steps}) {{\n{indent}    break;\n{indent}  }}\n"
            ));
        }
        out.push_str(&format!("{indent}}}\n"));
        out.push_str(&format!("{indent}trace.push(-{seen_var} - 100);\n"));
    }
}

pub(crate) fn collection_kind_strategy() -> impl Strategy<Value = CollectionKind> {
    prop_oneof![
        Just(CollectionKind::SetValues),
        Just(CollectionKind::SetEntries),
        Just(CollectionKind::MapKeys),
        Just(CollectionKind::MapValues),
        Just(CollectionKind::MapEntries),
    ]
}

fn in_loop_mutation_strategy() -> impl Strategy<Value = InLoopMutation> {
    prop_oneof![
        4 => Just(InLoopMutation::None),
        2 => Just(InLoopMutation::DeleteCurrent),
        2 => Just(InLoopMutation::DeleteAndReinsert),
        2 => (10..=60i32).prop_map(InLoopMutation::InsertOffset),
        1 => (10..=60i32).prop_map(InLoopMutation::ClearAndInsert),
    ]
}

fn action_strategy() -> impl Strategy<Value = CollectionAction> {
    prop_oneof![
        2 => (0..NUM_ITER_SLOTS).prop_map(|slot| CollectionAction::NewIter { slot }),
        5 => (0..NUM_ITER_SLOTS, 0..=4usize, in_loop_mutation_strategy())
            .prop_map(|(slot, steps, in_body)| CollectionAction::Step { slot, steps, in_body }),
        2 => in_loop_mutation_strategy()
            .prop_map(|in_body| CollectionAction::ForEach { in_body }),
        3 => (0..64i32, 0..10i32)
            .prop_map(|(key, val_bias)| CollectionAction::Insert { key, val_bias }),
        2 => (0..24i32, 32..=48usize)
            .prop_map(|(start_key, count)| CollectionAction::InsertBatch { start_key, count }),
        3 => (0..64i32).prop_map(|key| CollectionAction::Delete { key }),
        2 => (0..16i32, 32..=40usize)
            .prop_map(|(start_key, count)| CollectionAction::DeleteBatch { start_key, count }),
        1 => Just(CollectionAction::Clear),
        1 => (1..=6usize).prop_map(|count| CollectionAction::ChurnAbandoned { count }),
    ]
}

/// Strategy generating orthogonal collection + iterator state-machine programs.
pub(crate) fn collection_case_strategy() -> impl Strategy<Value = CollectionCase> {
    (
        collection_kind_strategy(),
        1..=40usize,
        prop::collection::vec(action_strategy(), 4..=16),
    )
        .prop_map(|(kind, initial_count, actions)| CollectionCase {
            kind,
            initial_count,
            actions,
            permuted: false,
        })
}

pub(crate) fn check(case: &Case, evidence: &Evidence) -> Result<()> {
    check_equivalence(case, evidence)?;
    if let Case::Collection(coll) = case
        && let Outcome::Values { observations, .. } = &evidence.component
    {
        let expected = coll.expected_observations();
        ensure!(
            observations == &expected,
            "collection state-machine observations diverged from Rust Vec reference model"
        );
    }
    Ok(())
}

pub(crate) fn contract() -> Contract {
    let witnesses = vec![
        Witness {
            partition: "set-paused-clear-and-reinsert",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::SetValues,
                initial_count: 8,
                actions: vec![
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::DeleteCurrent,
                    },
                    CollectionAction::Clear,
                    CollectionAction::Insert {
                        key: 42,
                        val_bias: 0,
                    },
                    CollectionAction::Insert {
                        key: 43,
                        val_bias: 0,
                    },
                    CollectionAction::Step {
                        slot: 0,
                        steps: 1,
                        in_body: InLoopMutation::InsertOffset(10),
                    },
                    CollectionAction::Step {
                        slot: 1,
                        steps: 2,
                        in_body: InLoopMutation::ClearAndInsert(20),
                    },
                ],
                permuted: false,
            }),
        },
        Witness {
            partition: "set-entries-and-foreach-mutation",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::SetEntries,
                initial_count: 6,
                actions: vec![
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::DeleteCurrent,
                    },
                    CollectionAction::ForEach {
                        in_body: InLoopMutation::DeleteAndReinsert,
                    },
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::InsertOffset(10),
                    },
                    CollectionAction::ForEach {
                        in_body: InLoopMutation::ClearAndInsert(30),
                    },
                ],
                permuted: false,
            }),
        },
        Witness {
            partition: "map-keys-compaction-under-paused-iterator",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::MapKeys,
                initial_count: 40,
                actions: vec![
                    CollectionAction::Step {
                        slot: 0,
                        steps: 3,
                        in_body: InLoopMutation::None,
                    },
                    CollectionAction::Step {
                        slot: 1,
                        steps: 1,
                        in_body: InLoopMutation::DeleteAndReinsert,
                    },
                    CollectionAction::DeleteBatch {
                        start_key: 0,
                        count: 35,
                    },
                    CollectionAction::Insert {
                        key: 99,
                        val_bias: 3,
                    },
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::None,
                    },
                ],
                permuted: false,
            }),
        },
        Witness {
            partition: "map-values-in-loop-mutations",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::MapValues,
                initial_count: 16,
                actions: vec![
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::DeleteAndReinsert,
                    },
                    CollectionAction::ForEach {
                        in_body: InLoopMutation::DeleteCurrent,
                    },
                    CollectionAction::DeleteBatch {
                        start_key: 2,
                        count: 14,
                    },
                    CollectionAction::InsertBatch {
                        start_key: 20,
                        count: 34,
                    },
                    CollectionAction::Step {
                        slot: 1,
                        steps: 2,
                        in_body: InLoopMutation::DeleteCurrent,
                    },
                    CollectionAction::DeleteBatch {
                        start_key: 20,
                        count: 33,
                    },
                ],
                permuted: false,
            }),
        },
        Witness {
            partition: "map-entries-abandoned-cursor-churn",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::MapEntries,
                initial_count: 36,
                actions: vec![
                    CollectionAction::ChurnAbandoned { count: 5 },
                    CollectionAction::Step {
                        slot: 0,
                        steps: 2,
                        in_body: InLoopMutation::InsertOffset(50),
                    },
                    CollectionAction::NewIter { slot: 1 },
                    CollectionAction::Step {
                        slot: 1,
                        steps: 1,
                        in_body: InLoopMutation::None,
                    },
                    CollectionAction::DeleteBatch {
                        start_key: 0,
                        count: 34,
                    },
                    CollectionAction::ChurnAbandoned { count: 4 },
                ],
                permuted: false,
            }),
        },
    ];
    Contract {
        id: "ecma.collection.iterators",
        description: "Live Set and Map iterators across active, paused, and abandoned lifetimes under delete compaction and clear",
        specification: "https://tc39.es/ecma262/#sec-keyed-collections",
        domain: "Orthogonal state-machine sequences of NewIter, partial Step and ForEach with in-body mutations, Insert/InsertBatch, Delete/DeleteBatch across the 32-tombstone compaction threshold, Clear, and abandoned cursor churn over Set.values, Set.entries, Map.keys, Map.values, and Map.entries; verified against both a Rust Vec reference model and Node",
        witnesses,
        strategy: collection_case_strategy()
            .prop_map(Case::Collection)
            .boxed(),
        check,
    }
}
