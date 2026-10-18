//! Orthogonal state-machine generator for live `Set` and `Map` iterators, checked against Node.
//!
//! Covers the product of:
//! - collection views (`Set.values()`, `Set.entries()`, `Map.keys()`, `Map.values()`,
//!   `Map.entries()`),
//! - multiple concurrent iterator lifetimes (active loop, paused via `break`, exhausted, and
//!   overwritten/abandoned),
//! - structural mutations both between loops and inside active loop bodies (`add`/`set`,
//!   `delete`, delete + reinsert, `clear`, and batch deletes across the 32-tombstone compaction
//!   threshold),
//! - declaration-order permutation (`permuted`).

use crate::{
    emit::{CHECKSUM_TS, Emitter},
    execution::{FactLimits, check_equivalence},
    registry::{Case, Contract, Witness},
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};
use std::fmt::Display;

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

    /// The iterator view over `coll`.
    fn view(self) -> &'static str {
        match self {
            Self::SetValues | Self::MapValues => "coll.values()",
            Self::MapKeys => "coll.keys()",
            Self::SetEntries | Self::MapEntries => "coll.entries()",
        }
    }

    /// The `for...of` binding over [`Self::view`]; entries destructure their pair.
    fn binding(self) -> &'static str {
        match self {
            Self::SetEntries | Self::MapEntries => "[k, v]",
            Self::MapValues => "v",
            Self::SetValues | Self::MapKeys => "k",
        }
    }

    /// The traced value of one `for...of` step.
    fn traced(self) -> &'static str {
        match self {
            Self::SetEntries | Self::MapEntries => "k + v",
            Self::MapValues => "v",
            Self::SetValues | Self::MapKeys => "k",
        }
    }

    /// The statement inserting `key`; maps store `valueFor(key, bias)`.
    fn insert(self, key: impl Display, bias: impl Display) -> String {
        if self.is_set() {
            format!("coll.add({key});")
        } else {
            format!("coll.set({key}, valueFor({key}, {bias}));")
        }
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
    #[serde(default)]
    pub fact_limits: FactLimits,
}

impl CollectionCase {
    /// Emit a standalone TypeScript module exporting `run(x: number): Observation`.
    pub fn source(&self) -> String {
        let mut e = Emitter::default();
        e.ordered(
            [
                "function normalizeBias(x: number): number { if (!Number.isFinite(x)) return 0; const t = Math.trunc(x); return t < -50 ? -50 : (t > 50 ? 50 : t); }",
                "function valueFor(k: number, bias: number): number { return k * 100 + bias; }",
                CHECKSUM_TS,
            ]
            .map(String::from),
            self.permuted,
        );
        e.line(format_args!(
            "export interface Observation {{ value: number; trace: number[]; }}"
        ));
        e.block(
            format_args!("export function run(x: number): Observation"),
            |e| self.emit_body(e),
        );
        e.finish()
    }

    fn emit_body(&self, e: &mut Emitter) {
        let kind = self.kind;
        let coll = if kind.is_set() {
            "new Set<number>()"
        } else {
            "new Map<number, number>()"
        };
        e.ordered(
            [
                "const bias = normalizeBias(x);".into(),
                "const trace: number[] = [];".into(),
                format!("const coll = {coll};"),
            ],
            self.permuted,
        );
        e.block(
            format_args!("for (let i = 0; i < {}; i += 1)", self.initial_count),
            |e| e.line(format_args!("{}", kind.insert("i", "bias"))),
        );
        e.ordered(
            (0..NUM_ITER_SLOTS).map(|slot| format!("let it{slot} = {};", kind.view())),
            self.permuted,
        );
        for (index, &action) in self.actions.iter().enumerate() {
            self.emit_action(e, index, action);
        }
        // Drain every retained slot so each paused iterator's remaining tail is observed.
        for slot in 0..NUM_ITER_SLOTS {
            self.emit_step(e, self.actions.len() + slot, slot, 0, InLoopMutation::None);
        }
        e.line(format_args!(
            "return {{ value: checksum(coll.size, trace), trace }};"
        ));
    }

    fn emit_action(&self, e: &mut Emitter, index: usize, action: CollectionAction) {
        let kind = self.kind;
        match action {
            CollectionAction::NewIter { slot } => e.line(format_args!(
                "it{} = {};",
                slot % NUM_ITER_SLOTS,
                kind.view()
            )),
            CollectionAction::Step {
                slot,
                steps,
                in_body,
            } => self.emit_step(e, index, slot % NUM_ITER_SLOTS, steps, in_body),
            CollectionAction::ForEach { in_body } => {
                let seen = format!("fe_seen_{index}");
                e.line(format_args!("let {seen} = 0;"));
                e.line(format_args!("coll.forEach((v, k) => {{"));
                e.indented(|e| {
                    e.line(format_args!("trace.push(k + v);"));
                    e.line(format_args!("{seen} += 1;"));
                    self.emit_first_mutation(e, &seen, in_body);
                });
                e.line(format_args!("}});"));
                e.line(format_args!("trace.push(-{seen} - 200);"));
            }
            CollectionAction::Insert { key, val_bias } => e.line(format_args!(
                "{}",
                kind.insert(key, format_args!("bias + {}", val_bias.rem_euclid(10)))
            )),
            CollectionAction::InsertBatch { start_key, count } => {
                e.block(format_args!("for (let b = 0; b < {count}; b += 1)"), |e| {
                    e.line(format_args!("const k = ({start_key}) + b;"));
                    e.line(format_args!("{}", kind.insert("k", "bias")));
                })
            }
            CollectionAction::Delete { key } => e.line(format_args!("coll.delete({key});")),
            CollectionAction::DeleteBatch { start_key, count } => e
                .block(format_args!("for (let b = 0; b < {count}; b += 1)"), |e| {
                    e.line(format_args!("coll.delete(({start_key}) + b);"))
                }),
            CollectionAction::Clear => e.line(format_args!("coll.clear();")),
            CollectionAction::ChurnAbandoned { count } => {
                e.block(format_args!("for (let c = 0; c < {count}; c += 1)"), |e| {
                    e.line(format_args!(
                        "for (let d = 0; d < 64; d += 1) {{ {} coll.delete(10000 + d); }}",
                        kind.insert("10000 + d", "d")
                    ));
                    e.line(format_args!("const tmpIt = {};", kind.view()));
                    e.block(
                        format_args!("for (const {} of tmpIt)", kind.binding()),
                        |e| e.line(format_args!("break;")),
                    );
                })
            }
        }
    }

    /// A `for...of` resumption of `it{slot}` that stops after `steps` items (0 drains it).
    fn emit_step(
        &self,
        e: &mut Emitter,
        index: usize,
        slot: usize,
        steps: usize,
        in_body: InLoopMutation,
    ) {
        let kind = self.kind;
        let seen = format!("seen_{index}");
        e.line(format_args!("let {seen} = 0;"));
        e.block(
            format_args!("for (const {} of it{slot})", kind.binding()),
            |e| {
                if kind == CollectionKind::MapValues {
                    e.line(format_args!("const k = Math.floor((v - bias) / 100);"));
                }
                e.line(format_args!("trace.push({});", kind.traced()));
                e.line(format_args!("{seen} += 1;"));
                self.emit_first_mutation(e, &seen, in_body);
                if steps > 0 {
                    e.block(format_args!("if ({seen} >= {steps})"), |e| {
                        e.line(format_args!("break;"))
                    });
                }
            },
        );
        e.line(format_args!("trace.push(-{seen} - 100);"));
    }

    /// `in_body` applied to the current key `k` on the loop's first iteration only.
    fn emit_first_mutation(&self, e: &mut Emitter, seen: &str, in_body: InLoopMutation) {
        let kind = self.kind;
        let body = |e: &mut Emitter| match in_body {
            InLoopMutation::None => {}
            InLoopMutation::DeleteCurrent => e.line(format_args!("coll.delete(k);")),
            InLoopMutation::DeleteAndReinsert => {
                e.line(format_args!("coll.delete(k);"));
                e.line(format_args!("{}", kind.insert("k", "bias + 1")));
            }
            InLoopMutation::InsertOffset(offset) | InLoopMutation::ClearAndInsert(offset) => {
                e.line(format_args!("const nk = k + ({offset});"));
                if matches!(in_body, InLoopMutation::ClearAndInsert(_)) {
                    e.line(format_args!("coll.clear();"));
                }
                e.line(format_args!("{}", kind.insert("nk", "bias")));
            }
        };
        if in_body != InLoopMutation::None {
            e.block(format_args!("if ({seen} === 1)"), body);
        }
    }
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
fn collection_case_strategy() -> impl Strategy<Value = CollectionCase> {
    (
        prop::sample::select(vec![
            CollectionKind::SetValues,
            CollectionKind::SetEntries,
            CollectionKind::MapKeys,
            CollectionKind::MapValues,
            CollectionKind::MapEntries,
        ]),
        1..=40usize,
        prop::collection::vec(action_strategy(), 4..=16),
        crate::strategies::fact_limits(),
    )
        .prop_map(
            |(kind, initial_count, actions, fact_limits)| CollectionCase {
                kind,
                initial_count,
                actions,
                permuted: false,
                fact_limits,
            },
        )
}

pub(crate) fn contract() -> Contract {
    let step = |slot, steps, in_body| CollectionAction::Step {
        slot,
        steps,
        in_body,
    };
    let witnesses = vec![
        Witness {
            partition: "set-paused-clear-foreach-and-reinsert",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::SetValues,
                initial_count: 8,
                actions: vec![
                    step(0, 2, InLoopMutation::DeleteCurrent),
                    CollectionAction::ForEach {
                        in_body: InLoopMutation::DeleteAndReinsert,
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
                    step(0, 1, InLoopMutation::InsertOffset(10)),
                    step(1, 2, InLoopMutation::ClearAndInsert(20)),
                ],
                permuted: false,
                fact_limits: FactLimits::Summarized,
            }),
        },
        Witness {
            partition: "map-entries-compaction-and-abandoned-cursor-churn",
            case: Case::Collection(CollectionCase {
                kind: CollectionKind::MapEntries,
                initial_count: 36,
                actions: vec![
                    CollectionAction::ChurnAbandoned { count: 5 },
                    step(0, 2, InLoopMutation::InsertOffset(50)),
                    CollectionAction::NewIter { slot: 1 },
                    step(1, 1, InLoopMutation::DeleteAndReinsert),
                    CollectionAction::DeleteBatch {
                        start_key: 0,
                        count: 34,
                    },
                    CollectionAction::InsertBatch {
                        start_key: 20,
                        count: 34,
                    },
                    CollectionAction::ForEach {
                        in_body: InLoopMutation::DeleteCurrent,
                    },
                    step(0, 2, InLoopMutation::None),
                    CollectionAction::ChurnAbandoned { count: 4 },
                ],
                permuted: false,
                fact_limits: FactLimits::Narrow,
            }),
        },
    ];
    Contract {
        id: "ecma.collection.iterators",
        description: "Live Set and Map iterators across active, paused, and abandoned lifetimes under delete compaction and clear",
        specification: "https://tc39.es/ecma262/#sec-keyed-collections",
        domain: "Orthogonal state-machine sequences of NewIter, partial Step and ForEach with in-body mutations, Insert/InsertBatch, Delete/DeleteBatch across the 32-tombstone compaction threshold, Clear, and abandoned cursor churn over Set.values, Set.entries, Map.keys, Map.values, and Map.entries; Node is the oracle",
        witnesses,
        strategy: collection_case_strategy()
            .prop_map(Case::Collection)
            .boxed(),
        check: check_equivalence,
    }
}
