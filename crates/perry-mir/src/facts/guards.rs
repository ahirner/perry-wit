use super::{Integer, Interval, Presence, ProgramFacts, Range, arithmetic_range};
use crate::{
    attrs::CompareKind,
    control::CondBrOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface},
        ops::FuncOp,
        types::FP64Type,
    },
    context::{Context, Ptr},
    graph::dominance::DomInfo,
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
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

pub(crate) struct GuardedFacts<'a> {
    ctx: &'a Context,
    facts: &'a ProgramFacts,
    cache: HashMap<(Ptr<BasicBlock>, Value), Range>,
    guards: HashMap<Value, Vec<(Ptr<BasicBlock>, Range)>>,
    branches: Vec<(Ptr<BasicBlock>, Value, bool)>,
}

impl<'a> GuardedFacts<'a> {
    pub(crate) fn new(ctx: &'a Context, facts: &'a ProgramFacts, function: &FuncOp) -> Self {
        let mut guards: HashMap<_, Vec<_>> = HashMap::new();
        let mut branches = Vec::new();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            if let Some(branch) = block
                .deref(ctx)
                .get_terminator(ctx)
                .and_then(|op| Operation::get_op::<CondBrOp>(op, ctx))
            {
                let operation = branch.get_operation().deref(ctx);
                if operation.get_successor(0) != operation.get_successor(1) {
                    for (index, target) in operation.successors().enumerate() {
                        if target.num_preds(ctx) == 1 {
                            let (condition, truth) = predicate(
                                ctx,
                                facts,
                                branch.get_operand_condition(ctx),
                                index == 0,
                            );
                            branches.push((target, condition, truth));
                        }
                    }
                }
            }
            for order in orders(ctx, block).into_iter().flatten() {
                // Both operands must exclude NaN before the false edge can order them.
                let (Range::Bounds(a), Range::Bounds(b)) = (
                    facts.range(ctx, order.smaller),
                    facts.range(ctx, order.larger),
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
            facts,
            guards,
            cache: HashMap::new(),
            branches,
        }
    }

    pub(crate) fn may_be_absent_at(
        &self,
        dominance: &mut DomInfo,
        consumer: Ptr<Operation>,
        value: Value,
        absence: crate::Absence,
    ) -> bool {
        self.presence(
            dominance,
            consumer
                .deref(self.ctx)
                .get_parent_block()
                .expect("consumer block"),
            value,
            32,
        )
        .admits(absence)
    }

    fn presence(
        &self,
        dominance: &mut DomInfo,
        block: Ptr<BasicBlock>,
        value: Value,
        depth: u8,
    ) -> Presence {
        let ctx = self.ctx;
        let mut presence = self.facts.presence(ctx, value);
        if depth == 0 {
            return presence;
        }
        if let Some(op) = value.defining_op()
            && Operation::is_op::<crate::OptionalCastOp>(op, ctx)
        {
            presence = presence.intersect(self.presence(
                dominance,
                block,
                op.deref(ctx).get_operand(0),
                depth - 1,
            ));
        }
        if let Some(definition) = value.defining_block()
            && definition.num_preds(ctx) != 0
        {
            let mut joined = Presence::default();
            for (terminator, index, operand) in incoming(ctx, definition, value.find_index(ctx)) {
                let predecessor = terminator
                    .deref(ctx)
                    .get_parent_block()
                    .expect("predecessor");
                let mut incoming = self.presence(dominance, predecessor, operand, depth - 1);
                if Operation::is_op::<CondBrOp>(terminator, ctx) {
                    incoming = self.refine(
                        dominance,
                        terminator.deref(ctx).get_operand(0),
                        index == 0,
                        operand,
                        incoming,
                        depth - 1,
                    );
                }
                joined = joined.join(incoming);
            }
            presence = presence.intersect(joined);
        }
        for &(target, condition, truth) in &self.branches {
            if target == block || dominance.block_strictly_dominates_block(ctx, target, block) {
                presence = self.refine(dominance, condition, truth, value, presence, depth - 1);
            }
        }
        presence
    }

    fn refine(
        &self,
        dominance: &mut DomInfo,
        condition: Value,
        truth: bool,
        value: Value,
        presence: Presence,
        depth: u8,
    ) -> Presence {
        if depth == 0 {
            return presence;
        }
        let ctx = self.ctx;
        let (condition, truth) = predicate(ctx, self.facts, condition, truth);
        if let Range::Bounds(Interval { lower, upper }) = self.facts.range(ctx, condition)
            && lower == upper
        {
            return if (lower != 0) == truth {
                presence
            } else {
                Presence::default()
            };
        }
        if let Some(op) = condition.defining_op() {
            let operation = op.deref(ctx);
            let absence = if Operation::is_op::<crate::IsNullOp>(op, ctx) {
                Some(crate::Absence::Null)
            } else if Operation::is_op::<crate::IsUndefinedOp>(op, ctx) {
                Some(crate::Absence::Undefined)
            } else {
                None
            };
            if let Some(absence) = absence {
                let tested = operation.get_operand(0);
                let possible = self.facts.presence(ctx, tested);
                if (truth && !possible.admits(absence))
                    || (!truth && possible.exclude(absence) == Presence::default())
                {
                    return Presence::default();
                }
                if self.facts.same_read(ctx, tested, value) {
                    return if truth {
                        presence.intersect(Presence::absent(absence))
                    } else {
                        presence.exclude(absence)
                    };
                }
            }
        }
        if let Some(block) = condition.defining_block()
            && block.num_preds(ctx) != 0
        {
            let mut joined = Presence::default();
            for (terminator, index, operand) in incoming(ctx, block, condition.find_index(ctx)) {
                let predecessor = terminator
                    .deref(ctx)
                    .get_parent_block()
                    .expect("predecessor");
                let (tested, required) = predicate(ctx, self.facts, operand, truth);
                if self.branches.iter().any(|&(target, guard, truth)| {
                    guard == tested
                        && truth != required
                        && (target == predecessor
                            || dominance.block_strictly_dominates_block(ctx, target, predecessor))
                }) {
                    continue;
                }
                let candidate = self.presence(dominance, predecessor, value, depth - 1);
                let mut candidate =
                    self.refine(dominance, operand, truth, value, candidate, depth - 1);
                if Operation::is_op::<CondBrOp>(terminator, ctx) {
                    candidate = self.refine(
                        dominance,
                        terminator.deref(ctx).get_operand(0),
                        index == 0,
                        value,
                        candidate,
                        depth - 1,
                    );
                }
                joined = joined.join(candidate);
            }
            return presence.intersect(joined);
        }
        presence
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
        let mut range = self.facts.range(ctx, value);
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

fn predicate(
    ctx: &Context,
    facts: &ProgramFacts,
    mut condition: Value,
    mut truth: bool,
) -> (Value, bool) {
    while let Some(compare) = condition
        .defining_op()
        .and_then(|op| Operation::get_op::<CompareOp>(op, ctx))
    {
        let equal = match compare.kind(ctx) {
            CompareKind::Eq => truth,
            CompareKind::Ne => !truth,
            _ => break,
        };
        let operation = compare.get_operation().deref(ctx);
        let (left, right) = (operation.get_operand(0), operation.get_operand(1));
        let Some((tested, constant)) =
            [(left, right), (right, left)]
                .into_iter()
                .find_map(|(tested, constant)| {
                    let Range::Bounds(Interval { lower, upper }) = facts.range(ctx, constant)
                    else {
                        return None;
                    };
                    (crate::scalar::is_bool(ctx, tested)
                        && lower == upper
                        && (lower == 0 || lower == 1))
                        .then_some((tested, lower != 0))
                })
        else {
            break;
        };
        condition = tested;
        truth = constant == equal;
    }
    (condition, truth)
}

fn incoming(
    ctx: &Context,
    block: Ptr<BasicBlock>,
    argument: usize,
) -> Vec<(Ptr<Operation>, usize, Value)> {
    let mut incoming = Vec::new();
    for predecessor in block.preds(ctx) {
        let terminator = predecessor
            .deref(ctx)
            .get_terminator(ctx)
            .expect("predecessor branch");
        let operation = Operation::get_op_dyn(terminator, ctx);
        let branch = op_cast::<dyn BranchOpInterface>(&*operation).expect("branch");
        for (index, successor) in terminator.deref(ctx).successors().enumerate() {
            if successor == block {
                incoming.push((
                    terminator,
                    index,
                    branch.successor_operands(ctx, index)[argument],
                ));
            }
        }
    }
    incoming
}
