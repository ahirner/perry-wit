//! Shared MIR live sets backed by pliron's SSA liveness and cached dominance.
use pliron::{
    analyses::liveness::{Liveness, LivenessTq},
    basic_block::BasicBlock,
    builtin::op_interfaces::OneRegionInterface,
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    irbuild::inserter::OpInsertionPoint,
    linked_list::ContainsLinkedList,
    operation::Operation,
    pass::{Analysis, AnalysisManager},
    result::Result,
    value::Value,
};
use std::collections::{HashMap, HashSet};

/// Block-local live intervals. Pliron supplies inter-block liveness; definition and
/// last-use positions answer local queries without copying a set at every operation.
#[derive(Default)]
pub struct LiveValues {
    positions: HashMap<Ptr<Operation>, (Ptr<BasicBlock>, usize)>,
    blocks: HashMap<Ptr<BasicBlock>, HashMap<Value, std::ops::Range<usize>>>,
    entry: HashMap<Ptr<BasicBlock>, HashSet<Value>>,
}

#[derive(Clone, Copy)]
pub struct LiveAfter<'a> {
    intervals: &'a HashMap<Value, std::ops::Range<usize>>,
    position: usize,
}

impl<'a> LiveAfter<'a> {
    pub fn values(self) -> impl Iterator<Item = Value> + 'a {
        self.intervals
            .iter()
            .filter_map(move |(&value, range)| range.contains(&self.position).then_some(value))
    }

    pub fn contains(&self, value: &Value) -> bool {
        self.intervals
            .get(value)
            .is_some_and(|range| range.contains(&self.position))
    }
}

impl LiveValues {
    pub fn after(&self, operation: Ptr<Operation>) -> LiveAfter<'_> {
        let (block, position) = self.positions[&operation];
        LiveAfter {
            intervals: &self.blocks[&block],
            position,
        }
    }

    /// Direct uses on this receiving edge, excluding values defined by the receiving block.
    pub fn on_entry(&self, block: Ptr<BasicBlock>, value: Value) -> bool {
        self.entry[&block].contains(&value)
    }
}

impl Analysis for LiveValues {
    fn name(&self) -> &str {
        "perry-live-values"
    }

    fn compute(
        root: Ptr<Operation>,
        ctx: &Context,
        analyses: &mut AnalysisManager,
    ) -> Result<Self> {
        analyses.get_analysis::<Liveness<LivenessTq>>(root, ctx)?;
        let mut liveness = analyses
            .try_get_analysis_mut::<Liveness<LivenessTq>>(root)
            .expect("SSA liveness");
        let mut dominance = analyses
            .try_get_analysis_mut::<DomInfo>(root)
            .expect("liveness computes dominance");
        let mut result = Self::default();
        for function in crate::aggregates::functions(root, ctx) {
            let region = function.get_region(ctx);
            let mut definitions = HashMap::new();
            for block in region.deref(ctx).iter(ctx) {
                for value in block.deref(ctx).arguments() {
                    definitions.insert(value, (block, 0));
                }
                for (index, op) in block.deref(ctx).iter(ctx).enumerate() {
                    let position = index + 1;
                    result.positions.insert(op, (block, position));
                    for value in op.deref(ctx).results() {
                        definitions.insert(value, (block, position));
                    }
                }
            }
            let mut crossing = HashSet::new();
            for block in region.deref(ctx).iter(ctx) {
                for op in block.deref(ctx).iter(ctx) {
                    for value in op.deref(ctx).operands() {
                        if definitions[&value].0 != block {
                            crossing.insert(value);
                        }
                    }
                }
            }
            for block in region.deref(ctx).iter(ctx) {
                let mut intervals = HashMap::new();
                for &value in &crossing {
                    if liveness.is_live_at_point(
                        ctx,
                        &mut dominance,
                        value,
                        OpInsertionPoint::AtBlockEnd(block),
                    ) {
                        let (definition, position) = definitions[&value];
                        let start = if definition == block { position } else { 0 };
                        intervals.insert(value, start..usize::MAX);
                    }
                }
                for (index, op) in block.deref(ctx).iter(ctx).enumerate() {
                    for value in op.deref(ctx).operands() {
                        let (definition, position) = definitions[&value];
                        let start = if definition == block { position } else { 0 };
                        intervals
                            .entry(value)
                            .and_modify(|range| range.end = range.end.max(index + 1))
                            .or_insert(start..index + 1);
                    }
                }
                let entry = intervals
                    .iter()
                    .filter_map(|(&value, range)| {
                        (range.start == 0 && definitions[&value].0 != block).then_some(value)
                    })
                    .collect();
                result.entry.insert(block, entry);
                result.blocks.insert(block, intervals);
            }
        }
        Ok(result)
    }
}
