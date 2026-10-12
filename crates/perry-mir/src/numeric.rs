//! WIT-seeded, context-sensitive numeric constraints over SSA values.
//!
//! Contexts contain domains, never constants or intervals. A single worklist propagates
//! representation facts forward and required widths backward, including recursive calls.
//! Rewriting runs only after the analysis converges, fixing semantic wrap points before
//! SCCP, inlining and other algebraic transformations.
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp},
    control::ReturnOp,
    integers::BigIntConstOp,
    intrinsics::PromiseResolvedOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{FPDoubleAttr, IdentifierAttr, IntegerAttr, TypeAttr},
        op_interfaces::{
            BranchOpInterface, OneRegionInterface, OneResultInterface, SymbolOpInterface,
        },
        ops::{ConstantOp, FuncOp},
        type_interfaces::FunctionTypeInterface,
        types::{FunctionType, IntegerType, Signedness},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    irbuild::{
        IRStatus,
        cloning::{IrMapping, clone_operation},
        inserter::{Inserter, OpInsertionPoint},
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    operation::Operation,
    pass::{Analysis, AnalysisManager, Pass, PassResult},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
    value::Value,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

pliron::dict_key!(WIT_SIGNATURE, "perry_wit_signature");
pliron::dict_key!(SPECIALIZATION, "perry_specialization");

/// Attach validated WIT numeric constraints before optimizing a source function.
pub fn constrain(
    ctx: &mut Context,
    function: &FuncOp,
    args: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
) {
    let signature = FunctionType::get(ctx, args, results);
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(WIT_SIGNATURE.clone(), TypeAttr::new(signature.into()));
}

/// Implementation selected for a constrained boundary, or the original function.
pub fn implementation(ctx: &Context, function: &FuncOp) -> Identifier {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<IdentifierAttr>(&SPECIALIZATION)
        .map(|name| name.clone().into())
        .unwrap_or_else(|| function.get_symbol_name(ctx))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Integer {
    bits: u32,
    signed: bool,
}

impl Integer {
    fn bounds(self) -> Interval {
        if self.signed {
            Interval {
                lower: -(1i128 << (self.bits - 1)),
                upper: (1i128 << (self.bits - 1)) - 1,
            }
        } else {
            Interval {
                lower: 0,
                upper: (1i128 << self.bits) - 1,
            }
        }
    }
    fn from_type(ctx: &Context, ty: TypeHandle) -> Option<Self> {
        let ty = ty.deref(ctx);
        let ty = ty.downcast_ref::<IntegerType>()?;
        (matches!(ty.width(), 8 | 16 | 32 | 64) && ty.signedness() != Signedness::Signless)
            .then_some(Self {
                bits: ty.width(),
                signed: ty.signedness() != Signedness::Unsigned,
            })
    }

    fn ty(self, ctx: &mut Context) -> TypeHandle {
        IntegerType::get(
            ctx,
            self.bits,
            if self.signed {
                Signedness::Signed
            } else {
                Signedness::Unsigned
            },
        )
        .into()
    }

    fn common(self, other: Self) -> Self {
        if self.signed == other.signed {
            return Self {
                bits: self.bits.max(other.bits),
                signed: self.signed,
            };
        }
        let (signed, unsigned) = if self.signed {
            (self, other)
        } else {
            (other, self)
        };
        if signed.bits > unsigned.bits {
            return signed;
        }
        [8, 16, 32, 64]
            .into_iter()
            .find(|&bits| bits > unsigned.bits)
            .map(|bits| Self { bits, signed: true })
            .unwrap_or(Self {
                bits: 64,
                signed: false,
            })
    }

    fn unconstrained(self) -> Self {
        Self { bits: 64, ..self }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Interval {
    lower: i128,
    upper: i128,
}

impl Interval {
    fn fits(self, domain: Integer) -> bool {
        let bounds = domain.bounds();
        self.lower >= bounds.lower && self.upper <= bounds.upper
    }

    fn include(self, domain: Integer) -> Option<Integer> {
        if self.fits(domain) {
            return Some(domain);
        }
        for bits in [8, 16, 32, 64] {
            for signed in [true, false] {
                let common = domain.common(Integer { bits, signed });
                if self.fits(common) {
                    return Some(common);
                }
            }
        }
        None
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Range {
    #[default]
    Bottom,
    Bounds(Interval),
    Unknown,
}

impl Range {
    fn join(self, other: Self, domain: Option<Integer>) -> Self {
        match (self, other) {
            (Self::Bottom, range) | (range, Self::Bottom) => range,
            (Self::Unknown, _) | (_, Self::Unknown) => Self::Unknown,
            (Self::Bounds(old), Self::Bounds(new)) => {
                let limits = domain.map_or(
                    Interval {
                        lower: i128::MIN,
                        upper: i128::MAX,
                    },
                    Integer::bounds,
                );
                Self::Bounds(Interval {
                    lower: if new.lower < old.lower {
                        limits.lower
                    } else {
                        old.lower
                    },
                    upper: if new.upper > old.upper {
                        limits.upper
                    } else {
                        old.upper
                    },
                })
            }
        }
    }
}

/// Literal integer values are eligible for specialization, but do not themselves impose
/// wrapping semantics on an unconstrained TypeScript computation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fact {
    #[default]
    Bottom,
    Literal,
    Integer(Integer),
    Original,
}

impl Fact {
    fn join(self, other: Self) -> Self {
        match (self, other) {
            (Self::Bottom, fact) | (fact, Self::Bottom) => fact,
            (Self::Original, _) | (_, Self::Original) => Self::Original,
            (Self::Literal, fact) | (fact, Self::Literal) => fact,
            (Self::Integer(a), Self::Integer(b)) => Self::Integer(a.common(b)),
        }
    }

    fn integer(self) -> Option<Integer> {
        if let Self::Integer(integer) = self {
            Some(integer)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ResultDomain {
    Inferred,
    Integer(Integer),
    /// Escaping promises keep their declared payload representation; their callees may
    /// still specialize arithmetic using known argument domains.
    Original,
}

impl ResultDomain {
    fn integer(self) -> Option<Integer> {
        match self {
            Self::Integer(integer) => Some(integer),
            Self::Inferred | Self::Original => None,
        }
    }
}

impl From<Option<Integer>> for ResultDomain {
    fn from(integer: Option<Integer>) -> Self {
        integer.map_or(Self::Inferred, Self::Integer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    function: Identifier,
    args: Vec<Option<Integer>>,
    result: ResultDomain,
    /// Actual MIR ranges never assume a specialization that has not been rewritten yet.
    specialize: bool,
}

#[derive(Clone, Copy)]
enum Constraint {
    Op(Ptr<Operation>),
    Edge { from: Value, to: Value },
}

struct Body {
    function: FuncOp,
    constraints: Vec<Constraint>,
    dependents: HashMap<Value, Vec<usize>>,
    parameters: Vec<Value>,
}

impl Body {
    fn new(ctx: &Context, function: FuncOp) -> Self {
        let mut constraints = Vec::new();
        let mut dependents: HashMap<Value, Vec<usize>> = HashMap::new();
        for op in crate::aggregates::operations(ctx, &function) {
            let index = constraints.len();
            constraints.push(Constraint::Op(op));
            for value in op.deref(ctx).operands().chain(op.deref(ctx).results()) {
                dependents.entry(value).or_default().push(index);
            }
            let dynamic = Operation::get_op_dyn(op, ctx);
            if let Some(branch) = op_cast::<dyn BranchOpInterface>(&*dynamic) {
                for successor in 0..op.deref(ctx).get_num_successors() {
                    let block = op.deref(ctx).get_successor(successor);
                    for (to, from) in block
                        .deref(ctx)
                        .arguments()
                        .zip(branch.successor_operands(ctx, successor))
                    {
                        let index = constraints.len();
                        constraints.push(Constraint::Edge { from, to });
                        dependents.entry(from).or_default().push(index);
                        dependents.entry(to).or_default().push(index);
                    }
                }
            }
        }
        Self {
            function,
            constraints,
            dependents,
            parameters: function
                .get_entry_block(ctx)
                .deref(ctx)
                .arguments()
                .collect(),
        }
    }
}

#[derive(Default)]
struct State {
    facts: HashMap<Value, Fact>,
    ranges: HashMap<Value, Range>,
    required: HashMap<Value, Integer>,
    original_payloads: HashSet<Value>,
    calls: HashMap<Ptr<Operation>, usize>,
    result: Fact,
    result_range: Range,
    callers: BTreeSet<(usize, usize)>,
}

impl State {
    fn fact(&self, value: Value) -> Fact {
        self.facts.get(&value).copied().unwrap_or_default()
    }
    fn range(&self, value: Value) -> Range {
        self.ranges.get(&value).copied().unwrap_or_default()
    }
    fn representation(&self, value: Value) -> Option<Integer> {
        match self.fact(value) {
            Fact::Integer(integer) => Some(integer),
            Fact::Literal => self.required.get(&value).copied(),
            _ => None,
        }
    }
}

/// Cached result for one unchanged module. Domain propagation is Perry semantics;
/// scheduling, cloning, verification and invalidation use pliron infrastructure.
pub struct NumericAnalysis {
    bodies: HashMap<Identifier, Body>,
    keys: Vec<Key>,
    states: Vec<State>,
    roots: Vec<(FuncOp, usize)>,
    generic: HashMap<Identifier, usize>,
    pub constraint_visits: usize,
}

impl NumericAnalysis {
    /// Non-negative integral values, including loop-carried counts after range widening.
    pub(crate) fn is_count(&self, ctx: &Context, function: &FuncOp, value: Value) -> bool {
        let state = &self.states[self.generic[&function.get_symbol_name(ctx)]];
        matches!(
            state.range(value),
            Range::Bounds(Interval { lower: 0.., .. })
        )
    }
}

struct Solver {
    analysis: NumericAnalysis,
    contexts: HashMap<Key, usize>,
    pending: VecDeque<(usize, usize)>,
    queued: HashSet<(usize, usize)>,
}

impl Solver {
    fn enqueue(&mut self, item: (usize, usize)) {
        if self.queued.insert(item) {
            self.pending.push_back(item);
        }
    }

    fn context(&mut self, key: Key) -> usize {
        if let Some(&id) = self.contexts.get(&key) {
            return id;
        }
        let id = self.analysis.keys.len();
        let body = &self.analysis.bodies[&key.function];
        let mut state = State::default();
        for (&parameter, domain) in body.parameters.iter().zip(&key.args) {
            state
                .facts
                .insert(parameter, domain.map_or(Fact::Original, Fact::Integer));
            state.ranges.insert(
                parameter,
                domain.map_or(Range::Unknown, |domain| Range::Bounds(domain.bounds())),
            );
        }
        let constraints = body.constraints.len();
        self.contexts.insert(key.clone(), id);
        self.analysis.keys.push(key);
        self.analysis.states.push(state);
        for constraint in 0..constraints {
            self.enqueue((id, constraint));
        }
        // The signature is fixed for this context, even when requirements propagate
        // through the parameter: conversions belong at its uses, not at its entry.
        id
    }

    fn changed(&mut self, id: usize, value: Value) {
        let body = &self.analysis.bodies[&self.analysis.keys[id].function];
        let dependents = body.dependents.get(&value).cloned().unwrap_or_default();
        let parameter = body.parameters.contains(&value);
        for constraint in dependents {
            self.enqueue((id, constraint));
        }
        if parameter {
            let callers: Vec<_> = self.analysis.states[id].callers.iter().copied().collect();
            for caller in callers {
                self.enqueue(caller);
            }
        }
    }

    fn fact(&mut self, id: usize, value: Value, fact: Fact) {
        let old = self.analysis.states[id].fact(value);
        let new = old.join(fact);
        if new != old {
            self.analysis.states[id].facts.insert(value, new);
            self.changed(id, value);
        }
    }

    fn range(&mut self, id: usize, value: Value, range: Range) {
        let state = &self.analysis.states[id];
        let old = state.range(value);
        let new = old.join(range, state.representation(value));
        if old != new {
            self.analysis.states[id].ranges.insert(value, new);
            self.changed(id, value);
        }
    }

    fn require(&mut self, id: usize, value: Value, required: Option<Integer>) {
        let Some(required) = required else {
            return;
        };
        let old = self.analysis.states[id].required.get(&value).copied();
        let new = old.map_or(required, |old| old.common(required));
        if old != Some(new) {
            self.analysis.states[id].required.insert(value, new);
            self.changed(id, value);
        }
    }

    fn preserve_promise_payload(&mut self, id: usize, value: Value) {
        if self.analysis.states[id].original_payloads.insert(value) {
            self.fact(id, value, Fact::Original);
            self.changed(id, value);
        }
    }

    fn arithmetic(&self, ctx: &Context, id: usize, inputs: &[Value], result: Value) -> Fact {
        let state = &self.analysis.states[id];
        let facts: Vec<_> = inputs.iter().map(|&value| state.fact(value)).collect();
        if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
            return Fact::Integer(integer);
        }
        if facts.contains(&Fact::Original) {
            return Fact::Original;
        }
        if facts.contains(&Fact::Bottom) {
            return Fact::Bottom;
        }
        if !self.analysis.keys[id].specialize {
            return if facts.iter().all(|fact| *fact == Fact::Literal) {
                Fact::Literal
            } else {
                Fact::Original
            };
        }
        let input = facts
            .into_iter()
            .filter_map(Fact::integer)
            .reduce(Integer::common);
        let required = state.required.get(&result).copied();
        match (input, required) {
            (Some(input), Some(required)) => Fact::Integer(input.common(required)),
            (Some(input), None) => Fact::Integer(input),
            (None, Some(required)) => Fact::Integer(required),
            _ => Fact::Literal,
        }
    }

    fn process(&mut self, ctx: &Context, id: usize, index: usize) {
        let constraint = self.analysis.bodies[&self.analysis.keys[id].function].constraints[index];
        if let Constraint::Edge { from, to } = constraint {
            if from.get_type(ctx).deref(ctx).is::<PromiseType>()
                && (self.analysis.states[id].original_payloads.contains(&from)
                    || self.analysis.states[id].original_payloads.contains(&to)
                    || self.analysis.states[id].fact(from) == Fact::Original
                    || self.analysis.states[id].fact(to) == Fact::Original)
            {
                self.preserve_promise_payload(id, from);
                self.preserve_promise_payload(id, to);
            }
            let required = self.analysis.states[id].required.get(&to).copied();
            self.require(id, from, required);
            let fact = self.analysis.states[id].fact(from);
            self.fact(id, to, fact);
            self.range(id, to, self.analysis.states[id].range(from));
            return;
        }
        let Constraint::Op(op) = constraint else {
            unreachable!()
        };
        let inputs: Vec<_> = op.deref(ctx).operands().collect();
        let results: Vec<_> = op.deref(ctx).results().collect();
        let dynamic = Operation::get_op_dyn(op, ctx);
        if Operation::get_op::<AwaitOp>(op, ctx).is_none()
            && op_cast::<dyn BranchOpInterface>(&*dynamic).is_none()
        {
            for &value in &inputs {
                if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    self.preserve_promise_payload(id, value);
                }
            }
        }
        if let Some(index) = promise_payload(ctx, op) {
            let value = inputs[index];
            let promise = results[0];
            if self.analysis.states[id]
                .original_payloads
                .contains(&promise)
            {
                self.fact(id, promise, Fact::Original);
            } else {
                self.require(
                    id,
                    value,
                    self.analysis.states[id].required.get(&promise).copied(),
                );
                self.fact(id, promise, self.analysis.states[id].fact(value));
            }
            self.range(id, promise, self.analysis.states[id].range(value));
            return;
        }
        if Operation::get_op::<AwaitOp>(op, ctx).is_some() {
            let promise = inputs[0];
            let result = results[0];
            let fact = self.analysis.states[id].fact(promise);
            if fact == Fact::Original {
                self.preserve_promise_payload(id, promise);
            }
            if !self.analysis.states[id]
                .original_payloads
                .contains(&promise)
            {
                self.require(
                    id,
                    promise,
                    self.analysis.states[id].required.get(&result).copied(),
                );
            }
            self.fact(id, result, fact);
            self.range(id, result, self.analysis.states[id].range(promise));
            return;
        }
        if Operation::get_op::<ReturnOp>(op, ctx).is_some() {
            if let Some(&value) = inputs.first() {
                self.require(id, value, self.analysis.keys[id].result.integer());
                let fact = self.analysis.states[id].fact(value);
                let fact = if self.analysis.keys[id].result == ResultDomain::Original {
                    Fact::Original
                } else if fact == Fact::Literal {
                    self.analysis.keys[id]
                        .result
                        .integer()
                        .map_or(Fact::Literal, Fact::Integer)
                } else {
                    fact
                };
                let old = self.analysis.states[id].result;
                let new = old.join(fact);
                let old_range = self.analysis.states[id].result_range;
                let range = old_range.join(self.analysis.states[id].range(value), new.integer());
                if new != old || range != old_range {
                    self.analysis.states[id].result = new;
                    self.analysis.states[id].result_range = range;
                    let callers: Vec<_> =
                        self.analysis.states[id].callers.iter().copied().collect();
                    for caller in callers {
                        self.enqueue(caller);
                    }
                }
            }
            return;
        }
        let asynchronous = Operation::get_op::<AsyncCallOp>(op, ctx);
        if let Some(callee) = Operation::get_op::<CallOp>(op, ctx)
            .map(|call| call.callee(ctx))
            .or_else(|| asynchronous.map(|call| call.callee(ctx)))
        {
            if !self.analysis.keys[id].specialize
                && let Some(&child) = self.analysis.generic.get(&callee)
            {
                self.analysis.states[child].callers.insert((id, index));
                if let Some(&result) = results.first() {
                    self.fact(id, result, self.analysis.states[child].result);
                    self.range(id, result, self.analysis.states[child].result_range);
                }
                return;
            }
            let required = results
                .first()
                .and_then(|value| self.analysis.states[id].required.get(value))
                .copied();
            let facts: Vec<_> = inputs
                .iter()
                .map(|&value| self.analysis.states[id].fact(value))
                .collect();
            if facts.contains(&Fact::Bottom) {
                return;
            }
            if self.analysis.bodies.contains_key(&callee) {
                let preserve_result = asynchronous.is_some()
                    && self.analysis.states[id]
                        .original_payloads
                        .contains(&results[0]);
                let args = inputs
                    .iter()
                    .zip(&facts)
                    .map(|(value, fact)| {
                        if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                            None
                        } else {
                            match fact {
                                Fact::Integer(integer) => Some(*integer),
                                Fact::Literal => {
                                    let Range::Bounds(interval) =
                                        self.analysis.states[id].range(*value)
                                    else {
                                        return None;
                                    };
                                    interval.include(Integer {
                                        bits: 64,
                                        signed: true,
                                    })
                                }
                                _ => None,
                            }
                        }
                    })
                    .collect();
                let child = self.context(Key {
                    function: callee.clone(),
                    args,
                    result: if preserve_result {
                        ResultDomain::Original
                    } else {
                        required.into()
                    },
                    specialize: true,
                });
                self.analysis.states[id].calls.insert(op, child);
                self.analysis.states[child].callers.insert((id, index));
                let parameters = self.analysis.bodies[&callee].parameters.clone();
                for (input, parameter) in inputs.iter().zip(parameters) {
                    self.require(
                        id,
                        *input,
                        self.analysis.states[child]
                            .required
                            .get(&parameter)
                            .copied(),
                    );
                }
                if let Some(&result) = results.first() {
                    self.fact(id, result, self.analysis.states[child].result);
                    self.range(id, result, self.analysis.states[child].result_range);
                }
                return;
            }
        }
        if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx)
            && matches!(
                binary.kind(ctx),
                BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul
            )
        {
            let required = self.analysis.states[id].required.get(&results[0]).copied();
            for &input in &inputs {
                self.require(id, input, required);
            }
            self.fact(
                id,
                results[0],
                self.arithmetic(ctx, id, &inputs, results[0]),
            );
            self.range(
                id,
                results[0],
                arithmetic_range(
                    binary.kind(ctx),
                    self.analysis.states[id].range(inputs[0]),
                    self.analysis.states[id].range(inputs[1]),
                    self.analysis.states[id].fact(results[0]).integer(),
                ),
            );
            return;
        }
        if Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx)
            .is_some_and(|op| op.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::BigIntNegate))
        {
            let required = self.analysis.states[id].required.get(&results[0]).copied();
            self.require(id, inputs[0], required);
            self.fact(
                id,
                results[0],
                self.arithmetic(ctx, id, &inputs, results[0]),
            );
            self.range(
                id,
                results[0],
                arithmetic_range(
                    BinaryKind::Sub,
                    Range::Bounds(Interval { lower: 0, upper: 0 }),
                    self.analysis.states[id].range(inputs[0]),
                    self.analysis.states[id].fact(results[0]).integer(),
                ),
            );
            return;
        }
        if Operation::get_op::<CompareOp>(op, ctx).is_some()
            && let Some(domain) = comparison_domain(&self.analysis.states[id], &inputs)
        {
            for &input in &inputs {
                if self.analysis.states[id].fact(input) == Fact::Literal {
                    self.require(id, input, Some(domain));
                }
            }
        }
        for result in results {
            let fact = if literal(ctx, op).is_some() {
                Fact::Literal
            } else if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
                Fact::Integer(integer)
            } else {
                Fact::Original
            };
            self.fact(id, result, fact);
            let range = if let Some(range) = constant_range(ctx, op) {
                Range::Bounds(
                    self.analysis.states[id]
                        .representation(result)
                        .filter(|&domain| !range.fits(domain))
                        .map_or(range, Integer::bounds),
                )
            } else if Operation::get_op::<crate::collections::StringLenOp>(op, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayLenOp>(op, ctx).is_some()
                || Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx).is_some_and(|op| {
                    op.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::BytesLength)
                })
            {
                Range::Bounds(Interval {
                    lower: 0,
                    upper: u32::MAX as i128,
                })
            } else if let Some(integer) = fact.integer() {
                Range::Bounds(integer.bounds())
            } else if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
                self.analysis.states[id].range(inputs[0])
            } else {
                Range::Unknown
            };
            self.range(id, result, range);
        }
    }
}

fn promise_payload(ctx: &Context, op: Ptr<Operation>) -> Option<usize> {
    if Operation::get_op::<PromiseResolvedOp>(op, ctx).is_some() {
        Some(0)
    } else if Operation::get_op::<TimerOp>(op, ctx).is_some() {
        Some(1)
    } else {
        None
    }
}

fn constant_range(ctx: &Context, op: Ptr<Operation>) -> Option<Interval> {
    let value = if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        constant.magnitude(ctx)? as i128
    } else {
        let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
        let attr = constant.get_attr_builtin_constant_value(ctx)?;
        if let Some(integer) = attr.downcast_ref::<IntegerAttr>() {
            if integer.get_type().deref(ctx).signedness() == Signedness::Unsigned {
                integer.value().to_u64() as i128
            } else {
                integer.value().to_i64() as i128
            }
        } else {
            let value = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
            if !value.is_finite() || value.fract() != 0.0 {
                return None;
            }
            value as i128
        }
    };
    Some(Interval {
        lower: value,
        upper: value,
    })
}

fn arithmetic_range(kind: BinaryKind, lhs: Range, rhs: Range, domain: Option<Integer>) -> Range {
    let (a, b) = match (lhs, rhs) {
        (Range::Bottom, _) | (_, Range::Bottom) => return Range::Bottom,
        (Range::Bounds(a), Range::Bounds(b)) => (a, b),
        _ => return domain.map_or(Range::Unknown, |domain| Range::Bounds(domain.bounds())),
    };
    let interval = match kind {
        _ if domain.is_none()
            && [a.lower, a.upper, b.lower, b.upper]
                .iter()
                .any(|&bound| bound == i128::MIN || bound == i128::MAX)
            && !(kind == BinaryKind::Add
                && ((a.lower >= 0 && b.lower >= 0) || (a.upper <= 0 && b.upper <= 0))) =>
        {
            return Range::Unknown;
        }
        BinaryKind::Add => Interval {
            lower: a.lower.saturating_add(b.lower),
            upper: a.upper.saturating_add(b.upper),
        },
        BinaryKind::Sub => Interval {
            lower: a.lower.saturating_sub(b.upper),
            upper: a.upper.saturating_sub(b.lower),
        },
        BinaryKind::Mul => {
            let products: Option<Vec<_>> = [
                a.lower.checked_mul(b.lower),
                a.lower.checked_mul(b.upper),
                a.upper.checked_mul(b.lower),
                a.upper.checked_mul(b.upper),
            ]
            .into_iter()
            .collect();
            let Some(products) = products else {
                return domain.map_or(Range::Unknown, |domain| Range::Bounds(domain.bounds()));
            };
            Interval {
                lower: *products.iter().min().unwrap(),
                upper: *products.iter().max().unwrap(),
            }
        }
        _ => return Range::Unknown,
    };
    Range::Bounds(
        domain
            .filter(|&domain| !interval.fits(domain))
            .map_or(interval, Integer::bounds),
    )
}

fn comparison_domain(state: &State, inputs: &[Value]) -> Option<Integer> {
    let mut domain = inputs
        .iter()
        .filter_map(|&value| state.fact(value).integer())
        .reduce(Integer::common)?;
    for &input in inputs {
        match state.fact(input) {
            Fact::Integer(_) => {}
            Fact::Literal => {
                let Range::Bounds(interval) = state.range(input) else {
                    return None;
                };
                domain = interval.include(domain)?;
            }
            _ => return None,
        }
    }
    Some(domain)
}

fn literal(ctx: &Context, op: Ptr<Operation>) -> Option<u64> {
    if let Some(constant) = Operation::get_op::<BigIntConstOp>(op, ctx) {
        return constant.magnitude(ctx);
    }
    let constant = Operation::get_op::<ConstantOp>(op, ctx)?;
    let attr = constant.get_attr_builtin_constant_value(ctx)?;
    let value = f64::from(attr.downcast_ref::<FPDoubleAttr>()?.clone());
    (value.is_finite()
        && value.fract() == 0.0
        && !(value == 0.0 && value.is_sign_negative())
        && value >= i64::MIN as f64
        && value < 18446744073709551616.0)
        .then_some(value as i128 as u64)
}

impl Analysis for NumericAnalysis {
    fn name(&self) -> &str {
        "perry-numeric-domains"
    }

    fn compute(root: Ptr<Operation>, ctx: &Context, _: &mut AnalysisManager) -> Result<Self> {
        let functions = crate::aggregates::functions(root, ctx);
        let mut solver = Solver {
            analysis: Self {
                bodies: functions
                    .iter()
                    .map(|&function| (function.get_symbol_name(ctx), Body::new(ctx, function)))
                    .collect(),
                keys: Vec::new(),
                states: Vec::new(),
                roots: Vec::new(),
                generic: HashMap::new(),
                constraint_visits: 0,
            },
            contexts: HashMap::new(),
            pending: VecDeque::new(),
            queued: HashSet::new(),
        };
        for function in functions {
            let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?;
            let args = ty
                .deref(ctx)
                .arg_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect();
            let generic = solver.context(Key {
                function: function.get_symbol_name(ctx),
                args,
                result: ty
                    .deref(ctx)
                    .res_types()
                    .first()
                    .and_then(|&ty| Integer::from_type(ctx, ty))
                    .into(),
                specialize: false,
            });
            solver
                .analysis
                .generic
                .insert(function.get_symbol_name(ctx), generic);
            let signature = function
                .get_operation()
                .deref(ctx)
                .attributes
                .get::<TypeAttr>(&WIT_SIGNATURE)
                .map(|attr| attr.get_type(ctx));
            let Some(signature) = signature else {
                continue;
            };
            let signature = TypedHandle::<FunctionType>::from_handle(signature, ctx)?;
            let args = signature
                .deref(ctx)
                .arg_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect();
            let result = signature
                .deref(ctx)
                .res_types()
                .first()
                .and_then(|&ty| Integer::from_type(ctx, ty));
            let id = solver.context(Key {
                function: function.get_symbol_name(ctx),
                args,
                result: result.into(),
                specialize: true,
            });
            solver.analysis.roots.push((function, id));
        }
        loop {
            while let Some((id, index)) = solver.pending.pop_front() {
                solver.queued.remove(&(id, index));
                solver.analysis.constraint_visits += 1;
                solver.process(ctx, id, index);
            }
            let defaults: Vec<_> = solver
                .analysis
                .states
                .iter()
                .enumerate()
                .filter(|(id, _)| solver.analysis.keys[*id].specialize)
                .flat_map(|(id, state)| {
                    solver.analysis.bodies[&solver.analysis.keys[id].function]
                        .constraints
                        .iter()
                        .filter_map(move |constraint| {
                            let Constraint::Op(op) = *constraint else {
                                return None;
                            };
                            let value = op.deref(ctx).results().next()?;
                            let integer = state.fact(value).integer()?;
                            (Operation::get_op::<BinaryOp>(op, ctx).is_some()
                                && !value.get_type(ctx).deref(ctx).is::<IntegerType>()
                                && !state.required.contains_key(&value))
                            .then_some((id, value, integer.unconstrained()))
                        })
                })
                .collect();
            if defaults.is_empty() {
                break;
            }
            for (id, value, integer) in defaults {
                solver.require(id, value, Some(integer));
            }
        }
        Ok(solver.analysis)
    }
}

pub(crate) struct Specialize;

impl Pass for Specialize {
    fn name(&self) -> &str {
        "specialize-numeric-domains"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let analysis = analyses.get_analysis::<NumericAnalysis>(root, ctx)?;
        let specializations = if analysis.roots.is_empty() {
            0
        } else {
            rewrite(ctx, &analysis)
        };
        root.deref_mut(ctx).attributes.set(
            crate::pipeline::STATISTICS.clone(),
            crate::attrs::OptimizationStatistics {
                numeric_contexts: analysis.keys.len() as u64,
                constraint_visits: analysis.constraint_visits as u64,
                specializations: specializations as u64,
                cleanup_rounds: 0,
            },
        );
        let mut result = PassResult::default();
        result.ir_changed = if specializations == 0 {
            IRStatus::Unchanged
        } else {
            IRStatus::Changed
        };
        Ok(result)
    }
}

fn rewrite(ctx: &mut Context, analysis: &NumericAnalysis) -> usize {
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = analysis.roots.iter().map(|(_, id)| *id).collect();
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(analysis.states[id].calls.values().copied());
        }
    }
    let mut clones = HashMap::new();
    let mut signatures = HashMap::new();
    for (id, key) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let body = &analysis.bodies[&key.function];
        let mut mapping = IrMapping::new();
        let cloned = clone_operation(
            body.function.get_operation(),
            ctx,
            &mut rewriter,
            &mut mapping,
        );
        let function = Operation::get_op::<FuncOp>(cloned, ctx).expect("cloned function");
        let mut name: Identifier = format!("__perry_integer_{id}_{}", key.function)
            .try_into()
            .expect("generated identifier");
        while analysis.bodies.contains_key(&name) {
            name = format!("_{name}").try_into().expect("generated identifier");
        }
        function.set_symbol_name(ctx, name);
        cloned.deref_mut(ctx).attributes.0.remove(&WIT_SIGNATURE);
        cloned.insert_after(ctx, body.function.get_operation());
        let original = TypedHandle::<FunctionType>::from_handle(body.function.get_type(ctx), ctx)
            .expect("function type");
        let original_args = original.deref(ctx).arg_types();
        let args: Vec<_> = original_args
            .into_iter()
            .zip(&key.args)
            .map(|(ty, domain)| domain.map_or(ty, |domain| domain.ty(ctx)))
            .collect();
        let mut results = original.deref(ctx).res_types();
        if let Some(result) = results.first_mut()
            && let Some(domain) = analysis.states[id].result.integer()
        {
            *result = domain.ty(ctx);
        }
        let signature = FunctionType::get(ctx, args.clone(), results.clone());
        function.set_attr_builtin_func_type(ctx, TypeAttr::new(signature.into()));
        let arguments: Vec<_> = function
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        for (argument, ty) in arguments.into_iter().zip(&args) {
            argument.set_type(ctx, *ty);
        }
        signatures.insert(id, (args, results));
        clones.insert(id, (function, mapping));
    }
    for (id, _) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let (_, mapping) = &clones[&id];
        let state = &analysis.states[id];
        let body = &analysis.bodies[&analysis.keys[id].function];
        let values: Vec<_> = body
            .function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
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
        for value in values {
            if let Some(domain) = state.representation(value) {
                let cloned = mapping.lookup_value(value).expect("cloned value");
                let scalar = domain.ty(ctx);
                let ty = if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    PromiseType::get(ctx, scalar).into()
                } else {
                    scalar
                };
                cloned.set_type(ctx, ty);
            }
        }
        for original in crate::aggregates::operations(ctx, &body.function) {
            let op = mapping.lookup_op(original).expect("cloned operation");
            let inputs: Vec<_> = original.deref(ctx).operands().collect();
            let results: Vec<_> = original.deref(ctx).results().collect();
            rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(op));
            if let Some(&value) = results.first()
                && let Some(domain) = state.representation(value)
                && let Some(bits) = literal(ctx, original)
            {
                let ty = TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx)
                    .expect("integer type");
                let constant = ConstantOp::new(
                    ctx,
                    Box::new(IntegerAttr::new(
                        ty,
                        APInt::from_u64(bits, bw(domain.bits as usize)),
                    )),
                )
                .get_operation();
                rewriter.insert_operation(ctx, constant);
                rewriter.replace_operation(ctx, op, constant);
                continue;
            }
            let mut expected: Vec<_> = inputs.iter().map(|value| value.get_type(ctx)).collect();
            if let Some(intrinsic) =
                Operation::get_op::<crate::intrinsics::IntrinsicOp>(original, ctx)
                    .and_then(|op| op.intrinsic(ctx))
                && matches!(
                    intrinsic,
                    crate::intrinsics::Intrinsic::BigIntToNumber
                        | crate::intrinsics::Intrinsic::BigIntNegate
                )
            {
                let value = op.deref(ctx).get_operand(0);
                if Integer::from_type(ctx, value.get_type(ctx)).is_some()
                    && intrinsic == crate::intrinsics::Intrinsic::BigIntToNumber
                {
                    let ty = op.deref(ctx).get_result(0).get_type(ctx);
                    let cast = NumericCastOp::new(ctx, value, ty).get_operation();
                    rewriter.insert_operation(ctx, cast);
                    rewriter.replace_operation(ctx, op, cast);
                    continue;
                }
                if intrinsic == crate::intrinsics::Intrinsic::BigIntNegate
                    && let Some(domain) = state.representation(results[0])
                {
                    let ty = TypedHandle::<IntegerType>::from_handle(domain.ty(ctx), ctx)
                        .expect("integer type");
                    let zero = ConstantOp::new(
                        ctx,
                        Box::new(IntegerAttr::new(
                            ty,
                            APInt::from_u64(0, bw(domain.bits as usize)),
                        )),
                    );
                    rewriter.insert_operation(ctx, zero.get_operation());
                    let value = if value.get_type(ctx) == ty.into() {
                        value
                    } else {
                        let cast = NumericCastOp::new(ctx, value, ty.into());
                        rewriter.insert_operation(ctx, cast.get_operation());
                        cast.get_result(ctx)
                    };
                    let negated = BinaryOp::new(ctx, BinaryKind::Sub, zero.get_result(ctx), value)
                        .get_operation();
                    rewriter.insert_operation(ctx, negated);
                    rewriter.replace_operation(ctx, op, negated);
                    continue;
                }
            }
            if let Some(&child) = state.calls.get(&original) {
                let name = IdentifierAttr::new(clones[&child].0.get_symbol_name(ctx));
                let asynchronous = if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                    call.set_attr_perry_async_callee(ctx, name);
                    true
                } else {
                    Operation::get_op::<CallOp>(op, ctx)
                        .expect("call")
                        .set_attr_perry_callee(ctx, name);
                    false
                };
                expected.clone_from(&signatures[&child].0);
                let values: Vec<_> = op.deref(ctx).results().collect();
                for (value, ty) in values.into_iter().zip(&signatures[&child].1) {
                    let ty = if asynchronous {
                        PromiseType::get(ctx, *ty).into()
                    } else {
                        *ty
                    };
                    value.set_type(ctx, ty);
                }
            } else if Operation::get_op::<AwaitOp>(original, ctx).is_some() {
                expected[0] =
                    PromiseType::get(ctx, op.deref(ctx).get_result(0).get_type(ctx)).into();
            } else if let Some(index) = promise_payload(ctx, original) {
                expected[index] = TypedHandle::<PromiseType>::from_handle(
                    op.deref(ctx).get_result(0).get_type(ctx),
                    ctx,
                )
                .expect("promise")
                .deref(ctx)
                .value();
            } else if Operation::get_op::<BinaryOp>(original, ctx).is_some() {
                expected.fill(op.deref(ctx).get_result(0).get_type(ctx));
            } else if Operation::get_op::<CompareOp>(original, ctx).is_some() {
                if let Some(domain) = comparison_domain(state, &inputs) {
                    expected.fill(domain.ty(ctx));
                }
            } else if Operation::get_op::<ReturnOp>(original, ctx).is_some() {
                expected.clone_from(&signatures[&id].1);
            } else {
                let dynamic = Operation::get_op_dyn(op, ctx);
                if let Some(branch) = op_cast::<dyn BranchOpInterface>(&*dynamic) {
                    for successor in 0..op.deref(ctx).get_num_successors() {
                        let block = op.deref(ctx).get_successor(successor);
                        for (argument, index) in block
                            .deref(ctx)
                            .arguments()
                            .zip(branch.successor_operand_range(ctx, successor))
                        {
                            expected[index] = argument.get_type(ctx);
                        }
                    }
                }
            }
            for (index, ty) in expected.into_iter().enumerate() {
                let value = op.deref(ctx).get_operand(index);
                if value.get_type(ctx) != ty {
                    let cast = NumericCastOp::new(ctx, value, ty);
                    rewriter.insert_operation(ctx, cast.get_operation());
                    Operation::replace_operand(op, ctx, index, cast.get_result(ctx));
                }
            }
        }
    }
    for &(original, id) in &analysis.roots {
        let name = clones[&id].0.get_symbol_name(ctx);
        original
            .get_operation()
            .deref_mut(ctx)
            .attributes
            .set(SPECIALIZATION.clone(), IdentifierAttr::new(name));
    }
    reachable.len()
}
