use super::{Integer, Interval, MirInteger, Presence, ProgramFacts, Range, arithmetic_range};
use crate::{
    attrs::CompareKind,
    control::CondBrOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneOpdInterface, OneRegionInterface},
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
#[derive(Clone, Copy)]
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

/// The comparisons established by the two arms of a conditional branch.
pub(super) fn orders(ctx: &Context, branch: &CondBrOp) -> [Option<Order>; 2] {
    let terminator = branch.get_operation();
    let Some(compare) = branch
        .get_operand_condition(ctx)
        .defining_op()
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

/// Bounds the walk through loop-carried block arguments. Results are memoized per remaining
/// depth, so each answer is independent of query order and the walk stays polynomial.
const PRESENCE_DEPTH: u8 = 32;

/// Solved facts refined by the branch edges that dominate a use.
pub(crate) struct GuardedFacts<'a> {
    ctx: &'a Context,
    facts: &'a ProgramFacts,
    cache: HashMap<(Ptr<BasicBlock>, Value), Range>,
    orders: HashMap<Value, Vec<Order>>,
    /// Branch successors with a single predecessor, and the condition truth holding there.
    branches: Vec<(Ptr<BasicBlock>, Value, bool)>,
    states: HashMap<(Ptr<BasicBlock>, Value, u8), Presence>,
    restrictions: HashMap<(Value, bool, Value, u8), Presence>,
}

impl<'a> GuardedFacts<'a> {
    pub(crate) fn new(ctx: &'a Context, facts: &'a ProgramFacts, function: &FuncOp) -> Self {
        let mut by_operand: HashMap<_, Vec<_>> = HashMap::new();
        let mut branches = Vec::new();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            let Some(branch) = block
                .deref(ctx)
                .get_terminator(ctx)
                .and_then(|op| Operation::get_op::<CondBrOp>(op, ctx))
            else {
                continue;
            };
            for (index, target) in branch.get_operation().deref(ctx).successors().enumerate() {
                if target.num_preds(ctx) == 1 {
                    let (condition, truth) =
                        predicate(ctx, facts, branch.get_operand_condition(ctx), index == 0);
                    branches.push((target, condition, truth));
                }
            }
            for order in orders(ctx, &branch).into_iter().flatten() {
                for value in [order.smaller, order.larger] {
                    by_operand.entry(value).or_default().push(order);
                }
            }
        }
        Self {
            ctx,
            facts,
            orders: by_operand,
            cache: HashMap::new(),
            branches,
            states: HashMap::new(),
            restrictions: HashMap::new(),
        }
    }

    pub(crate) fn orders_at<'b>(
        &'b self,
        dominance: &'b mut DomInfo,
        block: Ptr<BasicBlock>,
        value: Value,
    ) -> impl Iterator<Item = Order> + 'b {
        self.orders
            .get(&value)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |order| dominates(self.ctx, dominance, order.target, block))
    }

    /// Possible states of the operand at a particular use.
    pub(crate) fn presence_at(
        &mut self,
        dominance: &mut DomInfo,
        block: Ptr<BasicBlock>,
        value: Value,
    ) -> Presence {
        self.states(dominance, block, value, PRESENCE_DEPTH)
    }

    /// The states `value` may hold in `block`: its declared states, narrowed through optional
    /// casts and unwraps, per-edge block arguments and the branch edges dominating `block`.
    fn states(
        &mut self,
        dominance: &mut DomInfo,
        block: Ptr<BasicBlock>,
        value: Value,
        depth: u8,
    ) -> Presence {
        let ctx = self.ctx;
        let mut states = self.facts.presence(ctx, value);
        if depth == 0 {
            return states;
        }
        if let Some(&cached) = self.states.get(&(block, value, depth)) {
            return cached;
        }
        let next = depth - 1;
        if let Some(source) = passed_through(ctx, value) {
            states = states.intersect(self.states(dominance, block, source, next));
        }
        if let Some(edges) = incoming(ctx, value) {
            let mut joined = Presence::default();
            for (predecessor, terminator, index, operand) in edges {
                let incoming = self.states(dominance, predecessor, operand, next);
                let edge = self.edge(dominance, terminator, index, operand, next);
                joined = joined.join(incoming.intersect(edge));
            }
            states = states.intersect(joined);
        }
        for index in 0..self.branches.len() {
            let (target, condition, truth) = self.branches[index];
            if dominates(ctx, dominance, target, block) {
                let allowed = self.restriction(dominance, condition, truth, value, next);
                states = states.intersect(allowed);
            }
        }
        self.states.insert((block, value, depth), states);
        states
    }

    /// The states `value` may hold after `terminator` takes successor `index`.
    fn edge(
        &mut self,
        dominance: &mut DomInfo,
        terminator: Ptr<Operation>,
        index: usize,
        value: Value,
        depth: u8,
    ) -> Presence {
        if let Some(branch) = Operation::get_op::<CondBrOp>(terminator, self.ctx) {
            let condition = branch.get_operand_condition(self.ctx);
            self.restriction(dominance, condition, index == 0, value, depth)
        } else {
            Presence::ANY
        }
    }

    /// The states `value` may hold where `tested` holds one of `holds`: `holds` when `tested`
    /// is the same read, empty when `tested` cannot hold any of them, and for a block argument
    /// the join over the incoming values that may flow into it.
    fn test(
        &mut self,
        dominance: &mut DomInfo,
        tested: Value,
        holds: Presence,
        value: Value,
        depth: u8,
    ) -> Presence {
        let ctx = self.ctx;
        let tested = through_unwraps(ctx, tested);
        if self.facts.presence(ctx, tested).intersect(holds).is_empty() {
            return Presence::default();
        }
        if self
            .facts
            .same_read(ctx, tested, through_unwraps(ctx, value))
        {
            return holds;
        }
        self.through_argument(
            dominance,
            tested,
            value,
            depth,
            |this, dom, operand, _, next| this.test(dom, operand, holds, value, next),
        )
    }

    /// For a block argument, join over its incoming edges the states of `value` at each
    /// predecessor, narrowed by the edge and by what `allowed` derives from the incoming value;
    /// [`Presence::ANY`] for any other value or once `depth` is spent.
    fn through_argument(
        &mut self,
        dominance: &mut DomInfo,
        argument: Value,
        value: Value,
        depth: u8,
        mut allowed: impl FnMut(&mut Self, &mut DomInfo, Value, Ptr<BasicBlock>, u8) -> Presence,
    ) -> Presence {
        let ctx = self.ctx;
        let (Some(next), Some(edges)) = (depth.checked_sub(1), incoming(ctx, argument)) else {
            return Presence::ANY;
        };
        let mut joined = Presence::default();
        for (predecessor, terminator, index, operand) in edges {
            let allowed = allowed(self, dominance, operand, predecessor, next);
            if allowed.is_empty() {
                continue;
            }
            let candidate = self.states(dominance, predecessor, value, next);
            let edge = self.edge(dominance, terminator, index, value, next);
            joined = joined.join(candidate.intersect(allowed).intersect(edge));
        }
        joined
    }

    /// The states `value` may hold where `condition` is `truth`: [`Presence::ANY`] when the
    /// condition says nothing about `value`, empty where the condition cannot be `truth`.
    fn restriction(
        &mut self,
        dominance: &mut DomInfo,
        condition: Value,
        truth: bool,
        value: Value,
        depth: u8,
    ) -> Presence {
        if depth == 0 {
            return Presence::ANY;
        }
        let ctx = self.ctx;
        let (condition, truth) = predicate(ctx, self.facts, condition, truth);
        let key = (condition, truth, value, depth);
        if let Some(&cached) = self.restrictions.get(&key) {
            return cached;
        }
        let allowed = if let Range::Bounds(Interval { lower, upper }) =
            self.facts.range(ctx, condition)
            && lower == upper
        {
            if (lower != 0) == truth {
                Presence::ANY
            } else {
                Presence::default()
            }
        } else if let Some((tested, absence)) = absence_test(ctx, condition) {
            let absent = Presence::absent(absence);
            let holds = if truth {
                absent
            } else {
                Presence::ANY.without(absent)
            };
            self.test(dominance, tested, holds, value, depth - 1)
        } else {
            // A boolean block argument (`&&`, `||`): join what each feasible incoming edge allows.
            let edge = |this: &mut Self, dom: &mut DomInfo, operand, predecessor, next| {
                let (tested, required) = predicate(ctx, this.facts, operand, truth);
                if this.branches.iter().any(|&(target, guard, holds)| {
                    guard == tested && holds != required && dominates(ctx, dom, target, predecessor)
                }) {
                    return Presence::default();
                }
                this.restriction(dom, operand, truth, value, next)
            };
            self.through_argument(dominance, condition, value, depth, edge)
        };
        self.restrictions.insert(key, allowed);
        allowed
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
        // Bounds for both operands exclude NaN, including on a false edge.
        if matches!(self.facts.range(ctx, value), Range::Bounds(_)) {
            for order in self.orders_at(dominance, block, value) {
                let other = if value == order.smaller {
                    order.larger
                } else {
                    order.smaller
                };
                if let Range::Bounds(bound) = self.facts.range(ctx, other) {
                    range = order.constrain(value, range, bound);
                }
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
        let Some((left, right)) = compared(ctx, &compare) else {
            break;
        };
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

fn dominates(
    ctx: &Context,
    dominance: &mut DomInfo,
    target: Ptr<BasicBlock>,
    block: Ptr<BasicBlock>,
) -> bool {
    target == block || dominance.block_strictly_dominates_block(ctx, target, block)
}

/// The two operands of `compare`.
fn compared(ctx: &Context, compare: &CompareOp) -> Option<(Value, Value)> {
    let operation = compare.get_operation().deref(ctx);
    let mut operands = operation.operands();
    Some((operands.next()?, operands.next()?))
}

/// The source of `value` when an unwrap or optional cast defines it.
fn passed_through(ctx: &Context, value: Value) -> Option<Value> {
    let op = value.defining_op()?;
    if let Some(unwrap) = Operation::get_op::<crate::UnwrapOp>(op, ctx) {
        return Some(unwrap.get_operand(ctx));
    }
    Operation::get_op::<crate::OptionalCastOp>(op, ctx).map(|cast| cast.get_operand(ctx))
}

/// The value an unwrap or optional cast passes through unchanged.
fn through_unwraps(ctx: &Context, mut value: Value) -> Value {
    while let Some(source) = passed_through(ctx, value) {
        value = source;
    }
    value
}

/// The operand and absence state an `is_null`/`is_undefined` test checks.
fn absence_test(ctx: &Context, condition: Value) -> Option<(Value, crate::Absence)> {
    let op = condition.defining_op()?;
    if let Some(test) = Operation::get_op::<crate::IsNullOp>(op, ctx) {
        return Some((test.get_operand(ctx), crate::Absence::Null));
    }
    Operation::get_op::<crate::IsUndefinedOp>(op, ctx)
        .map(|test| (test.get_operand(ctx), crate::Absence::Undefined))
}

/// An edge into a block: predecessor, its terminator, the successor index, and the operand.
type Edge = (Ptr<BasicBlock>, Ptr<Operation>, usize, Value);

/// Operands reaching a block argument. A function parameter or unknown branch supplies no proof.
fn incoming(ctx: &Context, value: Value) -> Option<Vec<Edge>> {
    let uses = value.defining_block()?.uses(ctx);
    if uses.is_empty() {
        return None;
    }
    let argument = value.find_index(ctx);
    uses.into_iter()
        .map(|edge| {
            let terminator = edge.user_op();
            let operation = Operation::get_op_dyn(terminator, ctx);
            let branch = op_cast::<dyn BranchOpInterface>(&*operation)?;
            let index = edge.find_index(ctx);
            let position = branch.successor_operand_range(ctx, index).nth(argument)?;
            let op = terminator.deref(ctx);
            Some((
                op.get_parent_block()?,
                terminator,
                index,
                op.get_operand(position),
            ))
        })
        .collect()
}

/// Join properties of definitions reachable before slot promotion.
pub(crate) fn join_origins<T>(
    ctx: &Context,
    value: Value,
    leaf: impl Fn(Ptr<Operation>) -> Option<T>,
    join: impl Fn(T, T) -> Option<T>,
) -> Option<T> {
    let mut seen = std::collections::HashSet::new();
    let mut pending = vec![value];
    let mut common = None;
    while let Some(value) = pending.pop() {
        if !seen.insert(value) {
            continue;
        }
        if value.defining_block().is_some() {
            let edges = incoming(ctx, value)?;
            pending.extend(edges.into_iter().map(|(_, _, _, operand)| operand));
            continue;
        }
        let op = value.defining_op()?;
        if let Some(source) = passed_through(ctx, value) {
            pending.push(source);
        } else if let Some(wrap) = Operation::get_op::<crate::WrapOp>(op, ctx) {
            pending.push(wrap.get_operand(ctx));
        } else if let Some(load) = Operation::get_op::<crate::LoadOp>(op, ctx) {
            let slot = load.slot(ctx);
            for use_ in slot.uses(ctx) {
                let user = use_.user_op();
                if Operation::get_op::<crate::LoadOp>(user, ctx).is_none() {
                    let store = Operation::get_op::<crate::StoreOp>(user, ctx)?;
                    if store.slot(ctx) != slot {
                        return None;
                    }
                    pending.push(store.value(ctx));
                }
            }
        } else {
            let fact = leaf(op)?;
            common = Some(match common {
                Some(previous) => join(previous, fact)?,
                None => fact,
            });
        }
    }
    common
}
