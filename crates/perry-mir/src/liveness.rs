//! Block-local live intervals from one backward bitset dataflow per function.
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

/// Live intervals of every block, stored flat. Positions count from the block's entry edge at
/// 0: block arguments are defined at 1 and operation `i` sits at `i + 1`. A value is live after
/// an operation when its interval contains the operation's position, and arrives on an
/// incoming edge when its interval starts at 0.
#[derive(Default)]
pub struct LiveValues {
    /// Each operation's block index and position.
    positions: HashMap<Ptr<Operation>, (u32, u32)>,
    blocks: HashMap<Ptr<BasicBlock>, u32>,
    /// Each block's span of `values` and `ranges`.
    spans: Vec<Range<u32>>,
    values: Vec<Value>,
    ranges: Vec<Range<u32>>,
}

#[derive(Clone, Copy)]
pub struct LiveAfter<'a> {
    values: &'a [Value],
    ranges: &'a [Range<u32>],
    position: u32,
}

impl<'a> LiveAfter<'a> {
    pub fn values(self) -> impl Iterator<Item = Value> + 'a {
        let position = self.position;
        self.values
            .iter()
            .zip(self.ranges)
            .filter_map(move |(&value, range)| range.contains(&position).then_some(value))
    }

    pub fn contains(&self, value: &Value) -> bool {
        self.values
            .iter()
            .zip(self.ranges)
            .any(|(live, range)| live == value && range.contains(&self.position))
    }
}

impl LiveValues {
    fn block(&self, block: u32) -> (&[Value], &[Range<u32>]) {
        let span = self.spans[block as usize].clone();
        let span = span.start as usize..span.end as usize;
        (&self.values[span.clone()], &self.ranges[span])
    }

    pub fn after(&self, operation: Ptr<Operation>) -> LiveAfter<'_> {
        let (block, position) = self.positions[&operation];
        let (values, ranges) = self.block(block);
        LiveAfter {
            values,
            ranges,
            position,
        }
    }

    /// Whether `value` reaches `block` on an incoming edge, being defined elsewhere.
    pub(crate) fn on_entry(&self, block: Ptr<BasicBlock>, value: Value) -> bool {
        let (values, ranges) = self.block(self.blocks[&block]);
        values
            .iter()
            .zip(ranges)
            .any(|(&live, range)| live == value && range.start == 0)
    }

    fn add_function(&mut self, ctx: &Context, blocks: &[Ptr<BasicBlock>]) {
        const NONE: u32 = u32::MAX;
        let first = self.spans.len() as u32;
        // Dense value indices with their defining block and position, as parallel arrays.
        let mut index = HashMap::new();
        let (mut values, mut def_block, mut def_position) = (Vec::new(), Vec::new(), Vec::new());
        let mut define = |value: Value, block: u32, position: u32| {
            index.insert(value, values.len() as u32);
            values.push(value);
            def_block.push(block);
            def_position.push(position);
        };
        for (id, &block) in (0..).zip(blocks) {
            self.blocks.insert(block, first + id);
            for argument in block.deref(ctx).arguments() {
                define(argument, id, 1);
            }
            for (position, op) in (1..).zip(block.deref(ctx).iter(ctx)) {
                self.positions.insert(op, (first + id, position));
                for result in op.deref(ctx).results() {
                    define(result, id, position);
                }
            }
        }
        // Uses as (position, value index) in program order, and values used across blocks.
        let mut uses = Vec::new();
        let mut use_spans = Vec::with_capacity(blocks.len());
        let mut crossing = vec![NONE; values.len()];
        let mut crossed = Vec::new();
        for (id, &block) in (0..).zip(blocks) {
            let start = uses.len();
            for (position, op) in (1..).zip(block.deref(ctx).iter(ctx)) {
                for operand in op.deref(ctx).operands() {
                    let value = index[&operand];
                    if def_block[value as usize] != id && crossing[value as usize] == NONE {
                        crossing[value as usize] = crossed.len() as u32;
                        crossed.push(value);
                    }
                    uses.push((position, value));
                }
            }
            use_spans.push(start..uses.len());
        }
        let successors: Vec<Vec<u32>> = blocks
            .iter()
            .map(|block| {
                let successors = block.deref(ctx).succs(ctx);
                successors.iter().map(|s| self.blocks[s] - first).collect()
            })
            .collect();
        // Live-in = upward-exposed uses | (live-out - definitions), as `blocks x words` bitsets.
        let words = crossed.len().div_ceil(64);
        let set = |bits: &mut [u64], block: usize, bit: u32| {
            bits[block * words + bit as usize / 64] |= 1 << (bit % 64);
        };
        let mut exposed = vec![0; blocks.len() * words];
        let mut defined = vec![0; blocks.len() * words];
        for (block, span) in use_spans.iter().enumerate() {
            for &(_, value) in &uses[span.clone()] {
                if def_block[value as usize] != block as u32 {
                    set(&mut exposed, block, crossing[value as usize]);
                }
            }
        }
        for (bit, &value) in (0..).zip(&crossed) {
            set(&mut defined, def_block[value as usize] as usize, bit);
        }
        let mut live_in = vec![0u64; blocks.len() * words];
        let mut live_out = vec![0u64; blocks.len() * words];
        let mut changed = true;
        while changed {
            changed = false;
            for block in (0..blocks.len()).rev() {
                let row = block * words..(block + 1) * words;
                for word in row.clone() {
                    let out = successors[block].iter().fold(0, |out, &s| {
                        out | live_in[s as usize * words + word - row.start]
                    });
                    let live = exposed[word] | (out & !defined[word]);
                    live_out[word] = out;
                    changed |= live != live_in[word];
                    live_in[word] = live;
                }
            }
        }
        // Intervals: live-out values run to the end, used values to their last use.
        let mut slot = vec![NONE; values.len()];
        for (block, span) in use_spans.into_iter().enumerate() {
            let id = block as u32;
            let start = self.values.len() as u32;
            let begin = |value: u32| {
                if def_block[value as usize] == id {
                    def_position[value as usize]
                } else {
                    0
                }
            };
            for (word, &bits) in live_out[block * words..(block + 1) * words]
                .iter()
                .enumerate()
            {
                let mut bits = bits;
                while bits != 0 {
                    let value = crossed[word * 64 + bits.trailing_zeros() as usize];
                    bits &= bits - 1;
                    slot[value as usize] = self.values.len() as u32;
                    self.values.push(values[value as usize]);
                    self.ranges.push(begin(value)..u32::MAX);
                }
            }
            // A value is dead after its last use, so a use at `position` ends the interval there.
            for &(position, value) in &uses[span] {
                let current = slot[value as usize];
                if current != NONE && current >= start {
                    let range = &mut self.ranges[current as usize];
                    range.end = range.end.max(position);
                } else {
                    slot[value as usize] = self.values.len() as u32;
                    self.values.push(values[value as usize]);
                    self.ranges.push(begin(value)..position);
                }
            }
            self.spans.push(start..self.values.len() as u32);
        }
    }
}

impl Analysis for LiveValues {
    fn name(&self) -> &str {
        "perry-live-values"
    }

    fn compute(
        root: Ptr<Operation>,
        ctx: &Context,
        _analyses: &mut AnalysisManager,
    ) -> Result<Self> {
        let mut result = Self::default();
        for function in crate::aggregates::functions(root, ctx) {
            let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
            result.add_function(ctx, &blocks);
        }
        Ok(result)
    }
}
