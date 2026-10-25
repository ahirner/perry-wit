use super::{Integer, Interval, ProgramFacts, Range, arithmetic_range};
use crate::{
    attrs::CompareKind,
    control::CondBrOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{op_interfaces::OneRegionInterface, ops::FuncOp, types::FP64Type},
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use std::collections::HashMap;

/// `smaller < larger` (`strict`) or `smaller <= larger` in `target`, a branch successor
/// without other predecessors. False edges hold only for operands that are not NaN.
pub(crate) struct Order {
    pub(crate) target: Ptr<BasicBlock>,
    pub(crate) smaller: Value,
    pub(crate) larger: Value,
    pub(crate) strict: bool,
}

impl Order {
    pub(super) fn constrain(&self, value: Value, range: Range, bound: Interval) -> Range {
        let gap = i128::from(self.strict);
        let interval = if value == self.smaller {
            Interval {
                lower: i128::MIN,
                upper: bound.upper.saturating_sub(gap),
            }
        } else if value == self.larger {
            Interval {
                lower: bound.lower.saturating_add(gap),
                upper: i128::MAX,
            }
        } else {
            return range;
        };
        range.intersect(Range::Bounds(interval))
    }
}

/// The orders both successors of a branch on `block`'s `<`, `<=`, `>` or `>=` comparison establish.
pub(crate) fn orders(ctx: &Context, block: Ptr<BasicBlock>) -> [Option<Order>; 2] {
    let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
        return [None, None];
    };
    let Some(compare) = Operation::get_op::<CondBrOp>(terminator, ctx)
        .and_then(|branch| branch.get_operand_condition(ctx).defining_op())
        .and_then(|op| Operation::get_op::<CompareOp>(op, ctx))
    else {
        return [None, None];
    };
    let operation = compare.get_operation().deref(ctx);
    let (x, y) = (operation.get_operand(0), operation.get_operand(1));
    let (smaller, larger, strict) = match compare.kind(ctx) {
        CompareKind::Lt => (x, y, true),
        CompareKind::Le => (x, y, false),
        CompareKind::Gt => (y, x, true),
        CompareKind::Ge => (y, x, false),
        CompareKind::Eq | CompareKind::Ne => return [None, None],
    };
    let [taken, fallthrough] = [0, 1].map(|edge| terminator.deref(ctx).get_successor(edge));
    if taken == fallthrough {
        return [None, None];
    }
    [
        (taken, smaller, larger, strict),
        (fallthrough, larger, smaller, !strict),
    ]
    .map(|(target, smaller, larger, strict)| {
        (target.num_preds(ctx) == 1).then_some(Order {
            target,
            smaller,
            larger,
            strict,
        })
    })
}

pub(crate) struct GuardedRanges<'a> {
    ctx: &'a Context,
    numeric: &'a ProgramFacts,
    cache: HashMap<(Ptr<BasicBlock>, Value), Range>,
    guards: HashMap<Value, Vec<(Ptr<BasicBlock>, Range)>>,
}

impl<'a> GuardedRanges<'a> {
    pub(crate) fn new(ctx: &'a Context, numeric: &'a ProgramFacts, function: &FuncOp) -> Self {
        let mut guards: HashMap<_, Vec<_>> = HashMap::new();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for order in orders(ctx, block).into_iter().flatten() {
                // Both operands must exclude NaN before the false edge can order them.
                let (Range::Bounds(a), Range::Bounds(b)) = (
                    numeric.range(ctx, order.smaller),
                    numeric.range(ctx, order.larger),
                ) else {
                    continue;
                };
                for (value, interval, bound) in [(order.smaller, a, b), (order.larger, b, a)] {
                    guards.entry(value).or_default().push((
                        order.target,
                        order.constrain(value, Range::Bounds(interval), bound),
                    ));
                }
            }
        }
        Self {
            ctx,
            numeric,
            guards,
            cache: HashMap::new(),
        }
    }

    pub(crate) fn at(
        &mut self,
        dominance: &mut DomInfo,
        consumer: Ptr<Operation>,
        value: Value,
    ) -> Option<(i128, i128)> {
        let block = consumer.deref(self.ctx).get_parent_block()?;
        match self.range(dominance, block, value, 32) {
            Range::Bounds(Interval { lower, upper }) => Some((lower, upper)),
            Range::Bottom | Range::Unknown => None,
        }
    }

    fn range(
        &mut self,
        dominance: &mut DomInfo,
        block: Ptr<BasicBlock>,
        value: Value,
        depth: u8,
    ) -> Range {
        if let Some(range) = self.cache.get(&(block, value)) {
            return *range;
        }
        let ctx = self.ctx;
        let mut range = self.numeric.range(ctx, value);
        if depth > 0
            && let Some(op) = value.defining_op()
        {
            let operand = |i| op.deref(ctx).get_operand(i);
            let derived = if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx) {
                let lhs = self.range(dominance, block, operand(0), depth - 1);
                let rhs = self.range(dominance, block, operand(1), depth - 1);
                arithmetic_range(binary.kind(ctx), lhs, rhs, None)
            } else if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
                self.range(dominance, block, operand(0), depth - 1)
            } else {
                Range::Unknown
            };
            let ty = value.get_type(ctx);
            let fits = if ty.deref(ctx).is::<FP64Type>() {
                derived.fits_exact_number()
            } else {
                Integer::from_type(ctx, ty).is_some_and(
                    |domain| matches!(derived, Range::Bounds(interval) if interval.fits(domain)),
                )
            };
            if fits {
                range = range.intersect(derived);
            }
        }
        for &(target, interval) in self.guards.get(&value).into_iter().flatten() {
            if target == block || dominance.block_strictly_dominates_block(ctx, target, block) {
                range = range.intersect(interval);
            }
        }
        self.cache.insert((block, value), range);
        range
    }
}
