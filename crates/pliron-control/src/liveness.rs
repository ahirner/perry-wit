//! Block-local live intervals, traced from SSA uses back to their definitions.
use pliron::{
    basic_block::BasicBlock,
    builtin::op_interfaces::OneRegionInterface,
    context::{Context, Ptr},
    linked_list::ContainsLinkedList,
    operation::Operation,
    pass::{Analysis, AnalysisManager},
    result::Result,
    value::Value,
};
use std::{collections::HashMap, ops::Range};

/// Position 0 is the incoming edge, 1 defines block arguments, and operation positions
/// start at 2. Each half-open interval ends at the last use or continues to a successor.
#[derive(Default)]
pub struct LiveValues {
    positions: HashMap<Ptr<Operation>, (usize, u32)>,
    blocks: HashMap<Ptr<BasicBlock>, usize>,
    spans: Vec<Range<usize>>,
    intervals: Vec<(Value, Range<u32>)>,
}

#[derive(Clone, Copy)]
pub struct LiveSet<'a> {
    intervals: &'a [(Value, Range<u32>)],
    position: u32,
}

impl<'a> LiveSet<'a> {
    /// Live values in definition order.
    pub fn values(self) -> impl Iterator<Item = Value> + 'a {
        self.intervals
            .iter()
            .filter_map(move |(value, range)| range.contains(&self.position).then_some(*value))
    }

    pub fn contains(self, value: &Value) -> bool {
        self.values().any(|live| live == *value)
    }
}

impl LiveValues {
    fn at(&self, block: usize, position: u32) -> LiveSet<'_> {
        LiveSet {
            intervals: &self.intervals[self.spans[block].clone()],
            position,
        }
    }

    pub fn after(&self, operation: Ptr<Operation>) -> LiveSet<'_> {
        let (block, position) = self.positions[&operation];
        self.at(block, position)
    }

    pub fn before(&self, operation: Ptr<Operation>) -> LiveSet<'_> {
        let (block, position) = self.positions[&operation];
        self.at(block, position - 1)
    }

    pub fn on_entry(&self, block: Ptr<BasicBlock>, value: Value) -> bool {
        self.at(self.blocks[&block], 0).contains(&value)
    }

    /// Analyze a function's selected blocks, ignoring predecessors and uses outside the selection.
    pub fn for_blocks(ctx: &Context, blocks: &[Ptr<BasicBlock>]) -> Self {
        let mut live = Self::default();
        live.add_function(ctx, blocks);
        live
    }

    fn add_function(&mut self, ctx: &Context, blocks: &[Ptr<BasicBlock>]) {
        let first = self.spans.len();
        let mut definitions = Vec::new();
        for (id, &block) in blocks.iter().enumerate() {
            self.blocks.insert(block, first + id);
            definitions.extend(block.deref(ctx).arguments().map(|arg| (arg, id, 1)));
            for (position, op) in (2..).zip(block.deref(ctx).iter(ctx)) {
                self.positions.insert(op, (first + id, position));
                definitions.extend(op.deref(ctx).results().map(|result| (result, id, position)));
            }
        }
        let predecessors: Vec<Vec<_>> = blocks
            .iter()
            .map(|b| {
                b.preds(ctx)
                    .into_iter()
                    .filter_map(|p| self.blocks.get(&p).map(|id| id - first))
                    .collect()
            })
            .collect();
        let mut intervals = vec![Vec::<(Value, Range<u32>)>::new(); blocks.len()];
        let mut work = Vec::new();
        for (value, definition, position) in definitions {
            work.extend(value.uses(ctx).into_iter().filter_map(|usage| {
                let &(block, end) = self.positions.get(&usage.user_op())?;
                Some((block - first, end))
            }));
            while let Some((block, end)) = work.pop() {
                let ranges = &mut intervals[block];
                if let Some((last, range)) = ranges.last_mut()
                    && *last == value
                {
                    range.end = range.end.max(end);
                    continue;
                }
                let start = if block == definition { position } else { 0 };
                ranges.push((value, start..end));
                if block != definition {
                    work.extend(predecessors[block].iter().map(|&pred| (pred, u32::MAX)));
                }
            }
        }
        for block in intervals {
            let start = self.intervals.len();
            self.intervals.extend(block);
            self.spans.push(start..self.intervals.len());
        }
    }
}

impl Analysis for LiveValues {
    fn name(&self) -> &str {
        "ssa-live-values"
    }

    fn compute(
        root: Ptr<Operation>,
        ctx: &Context,
        _analyses: &mut AnalysisManager,
    ) -> Result<Self> {
        let mut result = Self::default();
        for function in crate::functions(root, ctx) {
            let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
            result.add_function(ctx, &blocks);
        }
        Ok(result)
    }
}
