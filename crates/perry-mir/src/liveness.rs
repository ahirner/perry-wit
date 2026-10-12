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

/// Sets for one unchanged MIR snapshot. Pliron answers block-boundary queries; a local
/// backward scan derives operation sets without another CFG fixed-point solver.
#[derive(Default)]
pub struct LiveValues {
    after: HashMap<Ptr<Operation>, HashSet<Value>>,
    entry: HashMap<Ptr<BasicBlock>, HashSet<Value>>,
}

impl LiveValues {
    pub fn after(&self, operation: Ptr<Operation>) -> &HashSet<Value> {
        &self.after[&operation]
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
            let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
            let values: Vec<_> = blocks
                .iter()
                .flat_map(|block| {
                    block
                        .deref(ctx)
                        .arguments()
                        .chain(
                            block
                                .deref(ctx)
                                .iter(ctx)
                                .flat_map(|op| op.deref(ctx).results().collect::<Vec<_>>()),
                        )
                        .collect::<Vec<_>>()
                })
                .collect();
            for block in blocks {
                let mut live: HashSet<_> = values
                    .iter()
                    .copied()
                    .filter(|&value| {
                        liveness.is_live_at_point(
                            ctx,
                            &mut dominance,
                            value,
                            OpInsertionPoint::AtBlockEnd(block),
                        )
                    })
                    .collect();
                let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
                for op in ops.into_iter().rev() {
                    result.after.insert(op, live.clone());
                    for value in op.deref(ctx).results() {
                        live.remove(&value);
                    }
                    live.extend(op.deref(ctx).operands());
                }
                for argument in block.deref(ctx).arguments() {
                    live.remove(&argument);
                }
                result.entry.insert(block, live);
            }
        }
        Ok(result)
    }
}
