//! Cached numeric, runtime-effect and parameter-provenance constraints over SSA values.
//!
//! Contexts contain domains, never constants or intervals. A single worklist propagates
//! representation facts forward and required widths backward, including recursive calls.
//! Rewriting runs only after the analysis converges, fixing semantic wrap points before
//! SCCP, inlining and other algebraic transformations.
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp},
    control::ReturnOp,
    intrinsics::PromiseResolvedOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, TypeAttr},
        op_interfaces::{BranchOpInterface, SymbolOpInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::{FunctionType, IntegerType},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    op::{Op, op_cast},
    operation::Operation,
    pass::{Analysis, AnalysisManager},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

mod domains;
mod heap;
mod rewrite;
use domains::{
    Fact, Integer, Interval, Range, ResultDomain, arithmetic_range, comparison_domain,
    constant_range, literal,
};
pub(crate) use rewrite::Specialize;
mod outcomes;
use outcomes::{Outcomes, promise_payload};

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

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    function: Identifier,
    args: Vec<Option<Integer>>,
    results: Vec<ResultDomain>,
    /// Actual MIR ranges never assume a specialization that has not been rewritten yet.
    specialize: bool,
}

#[derive(Clone, Copy)]
enum Constraint {
    Op {
        op: Ptr<Operation>,
        heap: heap::OperationKind,
    },
    Edge {
        from: Value,
        to: Value,
    },
}

struct Body {
    function: FuncOp,
    constraints: Vec<Constraint>,
    dependents: HashMap<Value, Vec<usize>>,
    parameters: Vec<Value>,
    effects: crate::effects::LocalEffects,
    external: bool,
}

impl Body {
    fn new(ctx: &Context, function: FuncOp, effects: crate::effects::LocalEffects) -> Self {
        let mut constraints = Vec::new();
        let mut dependents: HashMap<Value, Vec<usize>> = HashMap::new();
        for op in crate::aggregates::operations(ctx, &function) {
            let index = constraints.len();
            constraints.push(Constraint::Op {
                op,
                heap: heap::OperationKind::classify(ctx, op),
            });
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
            external: crate::calls::is_exported(ctx, &function)
                || function
                    .get_operation()
                    .deref(ctx)
                    .attributes
                    .0
                    .contains_key(&WIT_SIGNATURE),
            function,
            effects,
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
    results: Vec<ResultFact>,
    callers: BTreeSet<(usize, usize)>,
    origins: HashMap<Value, BTreeSet<usize>>,
    summary: crate::effects::Summary,
    heap_values: HashMap<Value, usize>,
    outcomes: HashMap<Value, Outcomes>,
}

#[derive(Clone, Copy, Default)]
struct ResultFact {
    domain: Fact,
    range: Range,
    heap: Option<usize>,
    outcome: Outcomes,
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
pub struct ProgramFacts {
    bodies: HashMap<Identifier, Body>,
    keys: Vec<Key>,
    states: Vec<State>,
    roots: Vec<(FuncOp, usize)>,
    generic: HashMap<Identifier, usize>,
    pub constraint_visits: usize,
    /// Alias sets collapsed to a typed summary after exceeding 32 allocation sites.
    pub summarized_heap_sets: usize,
    heap: heap::HeapFacts,
}

impl ProgramFacts {
    pub fn effects(&self, function: &Identifier) -> Option<&crate::effects::Summary> {
        self.generic
            .get(function)
            .map(|&id| &self.states[id].summary)
    }

    /// Non-negative integral values, including loop-carried counts after range widening.
    pub(crate) fn is_count(&self, ctx: &Context, function: &FuncOp, value: Value) -> bool {
        let state = &self.states[self.generic[&function.get_symbol_name(ctx)]];
        crate::optional::optional_value(ctx, value.get_type(ctx)).is_none()
            && matches!(
                state.range(value),
                Range::Bounds(Interval { lower: 0.., .. })
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Operation(usize, usize),
    OpenObject(usize),
}

#[derive(Default)]
struct Worklist {
    pending: VecDeque<Work>,
    operations: Vec<Vec<bool>>,
    objects: Vec<bool>,
}

impl Worklist {
    fn enqueue(&mut self, item: Work) {
        let queued = match item {
            Work::Operation(context, index) => &mut self.operations[context][index],
            Work::OpenObject(object) => &mut self.objects[object],
        };
        if !*queued {
            *queued = true;
            self.pending.push_back(item);
        }
    }

    fn pop(&mut self) -> Option<Work> {
        let item = self.pending.pop_front()?;
        match item {
            Work::Operation(context, index) => self.operations[context][index] = false,
            Work::OpenObject(object) => self.objects[object] = false,
        }
        Some(item)
    }
}

struct Solver {
    analysis: ProgramFacts,
    contexts: HashMap<Key, usize>,
    work: Worklist,
    width_defaults: Vec<(usize, usize, Value)>,
}

impl Solver {
    fn context(&mut self, ctx: &Context, key: Key) -> usize {
        if let Some(&id) = self.contexts.get(&key) {
            return id;
        }
        let id = self.analysis.keys.len();
        let body = &self.analysis.bodies[&key.function];
        let mut state = State {
            results: vec![ResultFact::default(); key.results.len()],
            summary: crate::effects::Summary {
                effects: body.effects.effects,
                ..Default::default()
            },
            ..State::default()
        };
        for (position, (&parameter, domain)) in body.parameters.iter().zip(&key.args).enumerate() {
            if body.external {
                state
                    .outcomes
                    .insert(parameter, Outcomes::unknown(ctx, parameter.get_type(ctx)));
            }
            if crate::types::is_reference(ctx, parameter.get_type(ctx)) {
                state.origins.insert(parameter, BTreeSet::from([position]));
            }
            state
                .facts
                .insert(parameter, domain.map_or(Fact::Original, Fact::Integer));
            state.ranges.insert(
                parameter,
                domain.map_or(
                    if body.external {
                        Range::Unknown
                    } else {
                        Range::Bottom
                    },
                    |domain| Range::Bounds(domain.bounds()),
                ),
            );
        }
        let constraints = body.constraints.len();
        self.contexts.insert(key.clone(), id);
        self.analysis.keys.push(key);
        self.analysis.states.push(state);
        self.work.operations.push(vec![false; constraints]);
        for constraint in 0..constraints {
            self.work.enqueue(Work::Operation(id, constraint));
        }
        self.seed_heap_parameters(ctx, id);
        // The signature is fixed for this context, even when requirements propagate
        // through the parameter: conversions belong at its uses, not at its entry.
        id
    }

    fn changed(&mut self, id: usize, value: Value) {
        let body = &self.analysis.bodies[&self.analysis.keys[id].function];
        let dependents = body.dependents.get(&value).into_iter().flatten();
        let parameter = body.parameters.contains(&value);
        for &constraint in dependents {
            self.work.enqueue(Work::Operation(id, constraint));
        }
        if parameter {
            self.notify_callers(id);
        }
    }

    fn notify_callers(&mut self, id: usize) {
        for &caller in &self.analysis.states[id].callers {
            self.work.enqueue(Work::Operation(caller.0, caller.1));
        }
    }

    fn propagate_origins(&mut self, id: usize, from: Value, to: Value) {
        let origins = &mut self.analysis.states[id].origins;
        if from == to || !origins.contains_key(&from) {
            return;
        }
        origins.entry(to).or_default();
        let [Some(from), Some(target)] = origins.get_disjoint_mut([&from, &to]) else {
            unreachable!("origin entries exist")
        };
        let previous = target.len();
        target.extend(from.iter().copied());
        if previous != target.len() {
            self.changed(id, to);
        }
    }

    fn escape(&mut self, id: usize, value: Value) {
        let state = &mut self.analysis.states[id];
        let previous = state.summary.escapes.len();
        state
            .summary
            .escapes
            .extend(state.origins.get(&value).into_iter().flatten().copied());
        if previous != state.summary.escapes.len() {
            self.notify_callers(id);
        }
    }

    fn operation_effects(&mut self, ctx: &Context, id: usize, index: usize, op: Ptr<Operation>) {
        let operation = op.deref(ctx);
        if Operation::get_op::<AwaitOp>(op, ctx).is_some()
            && self.analysis.states[id]
                .outcome(operation.get_operand(0))
                .may_reject()
        {
            let current = self.analysis.states[id].summary.effects.failure;
            let failure = self.analysis.bodies[&self.analysis.keys[id].function]
                .effects
                .fail(current, op, true);
            if failure != current {
                self.analysis.states[id].summary.effects.failure = failure;
                self.notify_callers(id);
            }
        }
        for result in operation
            .results()
            .filter(|value| crate::types::is_reference(ctx, value.get_type(ctx)))
        {
            for input in operation.operands() {
                self.propagate_origins(id, input, result);
            }
        }
        if Operation::get_op::<ReturnOp>(op, ctx).is_some()
            || Operation::get_op::<crate::exceptions::RaiseOp>(op, ctx).is_some()
            || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx)
                .is_some_and(|op| op.phase(ctx) == crate::foreign::Phase::Start)
            || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
        {
            for value in operation.operands() {
                self.escape(id, value);
            }
        }
        let stored = if Operation::get_op::<crate::locals::StoreOp>(op, ctx).is_some()
            || Operation::get_op::<crate::heap::FieldSetOp>(op, ctx).is_some()
            || Operation::get_op::<crate::collections::ArrayPushOp>(op, ctx).is_some()
            || Operation::get_op::<crate::collections::ArrayExtendOp>(op, ctx).is_some()
            || Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx).is_some_and(|op| {
                op.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::ExceptionSetCause)
            })
        {
            Some(1)
        } else if Operation::get_op::<crate::collections::ArraySetOp>(op, ctx).is_some() {
            Some(2)
        } else {
            None
        };
        if let Some(stored) = stored {
            self.escape(id, operation.get_operand(stored));
        }
        let call = Operation::get_op::<CallOp>(op, ctx);
        let callee = call
            .map(|call| call.callee(ctx))
            .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|call| call.callee(ctx)));
        if let Some(callee) = callee {
            if let Some(&child) = self.analysis.generic.get(&callee) {
                self.analysis.states[child].callers.insert((id, index));
                let current = self.analysis.states[id].summary.effects;
                let effects = self.analysis.bodies[&self.analysis.keys[id].function]
                    .effects
                    .inherit(
                        current,
                        self.analysis.states[child].summary.effects,
                        op,
                        call.is_some(),
                    );
                if effects != current {
                    self.analysis.states[id].summary.effects = effects;
                    self.notify_callers(id);
                }
                for (position, value) in operation.operands().enumerate() {
                    if self.analysis.states[child]
                        .summary
                        .escapes
                        .contains(&position)
                    {
                        self.escape(id, value);
                    }
                }
            } else {
                for value in operation.operands() {
                    self.escape(id, value);
                }
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

    fn call_results(
        &mut self,
        ctx: &Context,
        id: usize,
        child: usize,
        op: Ptr<Operation>,
        asynchronous: bool,
    ) {
        for (position, result) in op.deref(ctx).results().enumerate() {
            let facts = self.analysis.states[child].results[position];
            let outcome = if asynchronous {
                if self.analysis.states[child]
                    .summary
                    .effects
                    .failure
                    .escapes()
                {
                    Outcomes::Either
                } else {
                    Outcomes::Fulfilled
                }
            } else {
                facts.outcome
            };
            self.outcome(id, result, outcome);
            self.fact(id, result, facts.domain);
            self.range(id, result, facts.range);
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

    fn process(&mut self, ctx: &Context, id: usize, index: usize) {
        let constraint = self.analysis.bodies[&self.analysis.keys[id].function].constraints[index];
        let (op, heap) = match constraint {
            Constraint::Edge { from, to } => {
                self.outcome(id, to, self.analysis.states[id].outcome(from));
                self.heap_alias(ctx, id, from, id, to, Work::Operation(id, index));
                self.propagate_origins(id, from, to);
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
            Constraint::Op { op, heap } => (op, heap),
        };
        self.operation_effects(ctx, id, index, op);
        let heap_range = self.heap_operation(ctx, id, index, op, heap);
        let operation = op.deref(ctx);
        let dynamic = Operation::get_op_dyn(op, ctx);
        if Operation::get_op::<AwaitOp>(op, ctx).is_none()
            && op_cast::<dyn BranchOpInterface>(&*dynamic).is_none()
        {
            for value in operation.operands() {
                if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    self.preserve_promise_payload(id, value);
                }
            }
        }
        if let Some(index) = promise_payload(ctx, op) {
            let value = operation.get_operand(index);
            let promise = operation.get_result(0);
            self.outcome(
                id,
                promise,
                if Operation::get_op::<PromiseResolvedOp>(op, ctx).is_some() {
                    Outcomes::Fulfilled
                } else {
                    Outcomes::Either
                },
            );
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
            let promise = operation.get_operand(0);
            let result = operation.get_result(0);
            self.outcome(id, result, Outcomes::unknown(ctx, result.get_type(ctx)));
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
            let mut changed = false;
            for (position, value) in operation.operands().enumerate() {
                let required = self.analysis.keys[id].results[position];
                self.require(id, value, required.integer());
                let fact = self.analysis.states[id].fact(value);
                let fact = if required == ResultDomain::Original {
                    Fact::Original
                } else if fact == Fact::Literal {
                    required.integer().map_or(Fact::Literal, Fact::Integer)
                } else {
                    fact
                };
                let old = self.analysis.states[id].results[position];
                let outcome = old.outcome.join(self.analysis.states[id].outcome(value));
                let domain = old.domain.join(fact);
                let range = old
                    .range
                    .join(self.analysis.states[id].range(value), domain.integer());
                if domain != old.domain || range != old.range || outcome != old.outcome {
                    self.analysis.states[id].results[position].domain = domain;
                    self.analysis.states[id].results[position].range = range;
                    self.analysis.states[id].results[position].outcome = outcome;
                    changed = true;
                }
            }
            if changed {
                self.notify_callers(id);
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
                self.heap_call(ctx, id, index, child, op);
                self.call_results(ctx, id, child, op, asynchronous.is_some());
                return;
            }
            if operation
                .operands()
                .any(|value| self.analysis.states[id].fact(value) == Fact::Bottom)
            {
                return;
            }
            if self.analysis.bodies.contains_key(&callee) {
                let preserve_result = asynchronous.is_some()
                    && self.analysis.states[id]
                        .original_payloads
                        .contains(&operation.get_result(0));
                let args = operation
                    .operands()
                    .map(|value| {
                        if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                            None
                        } else {
                            match self.analysis.states[id].fact(value) {
                                Fact::Integer(integer) => Some(integer),
                                Fact::Literal => {
                                    let Range::Bounds(interval) =
                                        self.analysis.states[id].range(value)
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
                let child = self.context(
                    ctx,
                    Key {
                        function: callee.clone(),
                        args,
                        results: operation
                            .results()
                            .map(|value| {
                                if preserve_result {
                                    ResultDomain::Original
                                } else {
                                    self.analysis.states[id]
                                        .required
                                        .get(&value)
                                        .copied()
                                        .into()
                                }
                            })
                            .collect(),
                        specialize: true,
                    },
                );
                self.analysis.states[id].calls.insert(op, child);
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op);
                for (position, input) in operation.operands().enumerate() {
                    let parameter = self.analysis.bodies[&callee].parameters[position];
                    self.require(
                        id,
                        input,
                        self.analysis.states[child]
                            .required
                            .get(&parameter)
                            .copied(),
                    );
                }
                self.call_results(ctx, id, child, op, asynchronous.is_some());
                return;
            }
        }
        if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx)
            && matches!(
                binary.kind(ctx),
                BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul
            )
        {
            let result = operation.get_result(0);
            let was_integer = self.analysis.states[id].fact(result).integer().is_some();
            let required = self.analysis.states[id].required.get(&result).copied();
            for input in operation.operands() {
                self.require(id, input, required);
            }
            self.fact(
                id,
                result,
                self.arithmetic(
                    ctx,
                    id,
                    &[operation.get_operand(0), operation.get_operand(1)],
                    result,
                ),
            );
            self.range(
                id,
                result,
                arithmetic_range(
                    binary.kind(ctx),
                    self.analysis.states[id].range(operation.get_operand(0)),
                    self.analysis.states[id].range(operation.get_operand(1)),
                    self.analysis.states[id].fact(result).integer(),
                ),
            );
            if !was_integer
                && self.analysis.keys[id].specialize
                && required.is_none()
                && !result.get_type(ctx).deref(ctx).is::<IntegerType>()
                && self.analysis.states[id].fact(result).integer().is_some()
            {
                self.width_defaults.push((id, index, result));
            }
            return;
        }
        if Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx)
            .is_some_and(|op| op.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::BigIntNegate))
        {
            let required = self.analysis.states[id]
                .required
                .get(&operation.get_result(0))
                .copied();
            self.require(id, operation.get_operand(0), required);
            self.fact(
                id,
                operation.get_result(0),
                self.arithmetic(
                    ctx,
                    id,
                    &[operation.get_operand(0)],
                    operation.get_result(0),
                ),
            );
            self.range(
                id,
                operation.get_result(0),
                arithmetic_range(
                    BinaryKind::Sub,
                    Range::Bounds(Interval { lower: 0, upper: 0 }),
                    self.analysis.states[id].range(operation.get_operand(0)),
                    self.analysis.states[id]
                        .fact(operation.get_result(0))
                        .integer(),
                ),
            );
            return;
        }
        if Operation::get_op::<CompareOp>(op, ctx).is_some()
            && let Some(domain) = comparison_domain(
                &self.analysis.states[id],
                &[operation.get_operand(0), operation.get_operand(1)],
            )
        {
            for input in operation.operands() {
                if self.analysis.states[id].fact(input) == Fact::Literal {
                    self.require(id, input, Some(domain));
                }
            }
        }
        for result in operation.results() {
            if Operation::get_op::<crate::intrinsics::PromiseRejectedOp>(op, ctx).is_some() {
                self.outcome(id, result, Outcomes::Rejected);
            } else if !matches!(
                heap,
                heap::OperationKind::Read { .. }
                    | heap::OperationKind::Alias { .. }
                    | heap::OperationKind::Combine(_)
            ) {
                self.outcome(id, result, Outcomes::unknown(ctx, result.get_type(ctx)));
            }
            let fact = if literal(ctx, op).is_some() {
                Fact::Literal
            } else if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
                Fact::Integer(integer)
            } else {
                Fact::Original
            };
            self.fact(id, result, fact);
            let range = if let Some(range) = heap_range {
                range
            } else if let Some(range) = constant_range(ctx, op) {
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
                self.analysis.states[id].range(operation.get_operand(0))
            } else {
                Range::Unknown
            };
            self.range(id, result, range);
        }
    }
}

impl Analysis for ProgramFacts {
    fn name(&self) -> &str {
        "perry-program-facts"
    }

    fn compute(root: Ptr<Operation>, ctx: &Context, _: &mut AnalysisManager) -> Result<Self> {
        let functions = crate::aggregates::functions(root, ctx);
        let effects = crate::effects::prepare(ctx, &functions)?;
        let mut solver = Solver {
            analysis: Self {
                bodies: functions
                    .iter()
                    .zip(effects)
                    .map(|(&function, effects)| {
                        (
                            function.get_symbol_name(ctx),
                            Body::new(ctx, function, effects),
                        )
                    })
                    .collect(),
                keys: Vec::new(),
                states: Vec::new(),
                roots: Vec::new(),
                generic: HashMap::new(),
                constraint_visits: 0,
                summarized_heap_sets: 0,
                heap: heap::HeapFacts::default(),
            },
            contexts: HashMap::new(),
            work: Worklist::default(),
            width_defaults: Vec::new(),
        };
        for function in functions {
            let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?;
            let args = ty
                .deref(ctx)
                .arg_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect();
            let generic = solver.context(
                ctx,
                Key {
                    function: function.get_symbol_name(ctx),
                    args,
                    results: ty
                        .deref(ctx)
                        .res_types()
                        .iter()
                        .map(|&ty| Integer::from_type(ctx, ty).into())
                        .collect(),
                    specialize: false,
                },
            );
            solver
                .analysis
                .generic
                .insert(function.get_symbol_name(ctx), generic);
            if function
                .get_operation()
                .deref(ctx)
                .attributes
                .0
                .contains_key(&SPECIALIZATION)
            {
                continue;
            }
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
            let results = signature
                .deref(ctx)
                .res_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty).into())
                .collect();
            let id = solver.context(
                ctx,
                Key {
                    function: function.get_symbol_name(ctx),
                    args,
                    results,
                    specialize: true,
                },
            );
            solver.analysis.roots.push((function, id));
        }
        loop {
            while let Some(item) = solver.work.pop() {
                solver.analysis.constraint_visits += 1;
                match item {
                    Work::Operation(id, index) => solver.process(ctx, id, index),
                    Work::OpenObject(object) => solver.open_object(ctx, object),
                }
            }
            // Every default is applied after propagation settles, before the next queue drain.
            solver
                .width_defaults
                .sort_unstable_by_key(|&(id, index, _)| std::cmp::Reverse((id, index)));
            let mut defaulted = false;
            while let Some((id, _, value)) = solver.width_defaults.pop() {
                solver.analysis.constraint_visits += 1;
                if !solver.analysis.states[id].required.contains_key(&value)
                    && let Some(integer) = solver.analysis.states[id].fact(value).integer()
                {
                    solver.require(id, value, Some(integer.unconstrained()));
                    defaulted = true;
                }
            }
            if !defaulted {
                break;
            }
        }
        Ok(solver.analysis)
    }
}
