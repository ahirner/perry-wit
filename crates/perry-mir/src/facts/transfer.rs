use super::{
    Body, Constraint, Fact, Integer, Interval, Key, Outcomes, Range, ResultDomain, Solver, Work,
    arithmetic_range, comparison_domain, constant_range, heap, literal, promise_payload,
};
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    control::ReturnOp,
    intrinsics::{BuiltinContract, Intrinsic, IntrinsicOp, PromiseRejectedOp, PromiseResolvedOp},
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{op_interfaces::BranchOpInterface, types::IntegerType},
    context::{Context, Ptr},
    op::op_cast,
    operation::Operation,
    r#type::Typed,
    value::Value,
};

impl Solver {
    pub(super) fn operation_effects(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        op: Ptr<Operation>,
    ) {
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
            || Operation::get_op::<IntrinsicOp>(op, ctx)
                .is_some_and(|op| op.intrinsic(ctx) == Some(Intrinsic::ExceptionSetCause))
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

    pub(super) fn fact(&mut self, id: usize, value: Value, fact: Fact) {
        let old = self.analysis.states[id].fact(value);
        let mut new = old.join(fact);
        if (!self.analysis.keys[id].specialize
            || self.analysis.states[id].original_payloads.contains(&value))
            && new == Fact::Numeric
        {
            new = Fact::Original;
        }
        if new != old {
            self.analysis.states[id].facts.insert(value, new);
            self.changed(id, value);
        }
    }

    pub(super) fn range(&mut self, id: usize, value: Value, range: Range) {
        let state = &self.analysis.states[id];
        let old = state.range(value);
        let expansions = state.range_expansions.get(&value).copied().unwrap_or(0);
        let (new, expanded, widened) =
            old.join_step(range, state.representation(value), expansions >= 1);
        if expanded {
            self.analysis.states[id]
                .range_expansions
                .insert(value, expansions.saturating_add(1));
        }
        if widened {
            self.analysis.range_widenings += 1;
        }
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

    pub(super) fn require(&mut self, id: usize, value: Value, required: Option<Integer>) {
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

    pub(super) fn constrain(&mut self, id: usize, value: Value, constrained: bool) {
        if constrained && self.analysis.states[id].constrained.insert(value) {
            self.changed(id, value);
        }
    }

    fn preserve_promise_payload(&mut self, id: usize, value: Value) {
        if self.analysis.states[id].original_payloads.insert(value) {
            self.fact(id, value, Fact::Original);
            self.changed(id, value);
        }
    }

    pub(super) fn process(&mut self, ctx: &Context, id: usize, index: usize) {
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
                let constrained = self.analysis.states[id].is_constrained(to);
                self.constrain(id, from, constrained);
                let fact = self.analysis.states[id].fact(from);
                self.fact(id, to, fact);
                self.range(id, to, self.analysis.states[id].range(from));
                return;
            }
            Constraint::Op { op, heap } => (op, heap),
        };
        self.operation_effects(ctx, id, index, op);
        let heap_facts = self.heap_operation(ctx, id, index, op, heap);
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
                let constrained = self.analysis.states[id].is_constrained(promise);
                self.constrain(id, value, constrained);
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
                let constrained = self.analysis.states[id].is_constrained(result);
                self.constrain(id, promise, constrained);
            }
            self.fact(id, result, fact);
            self.range(id, result, self.analysis.states[id].range(promise));
            return;
        }
        if Operation::get_op::<ReturnOp>(op, ctx).is_some() {
            let mut changed = false;
            let declared_results = self.analysis.bodies[&self.analysis.keys[id].function]
                .declared_results
                .clone();
            let is_summary = self.is_summary(id);
            for (position, value) in operation.operands().enumerate() {
                let required = self.analysis.keys[id].results[position];
                self.require(id, value, required.integer());
                self.constrain(
                    id,
                    value,
                    is_summary
                        || required.integer().is_some()
                        || declared_results.get(position).copied().flatten().is_some(),
                );
                let fact = self.analysis.states[id].fact(value);
                let fact = if required == ResultDomain::Original {
                    Fact::Original
                } else if let Some(declared) = declared_results.get(position).copied().flatten() {
                    Fact::Integer(declared)
                } else if matches!(fact, Fact::Literal { .. }) {
                    required
                        .integer()
                        .map_or(fact, |req| fact.join(Fact::Integer(req)))
                } else {
                    fact
                };
                let old = self.analysis.states[id].results[position];
                let outcome = old.outcome.join(self.analysis.states[id].outcome(value));
                let mut domain = old.domain.join(fact);
                if (!self.analysis.keys[id].specialize || required == ResultDomain::Original)
                    && domain == Fact::Numeric
                {
                    domain = Fact::Original;
                }
                let (range, expanded, widened) = old.range.join_step(
                    self.analysis.states[id].range(value),
                    domain.integer(),
                    old.expansions >= 1,
                );
                if expanded {
                    self.analysis.states[id].results[position].expansions =
                        old.expansions.saturating_add(1);
                }
                if widened {
                    self.analysis.range_widenings += 1;
                }
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
                let Body {
                    declared_args,
                    declared_results,
                    parameters,
                    ..
                } = &self.analysis.bodies[&callee];
                let declared_args = declared_args.clone();
                let declared_results = declared_results.clone();
                let parameters = parameters.clone();
                let args = operation
                    .operands()
                    .enumerate()
                    .map(|(position, value)| {
                        if let Some(declared) = declared_args.get(position).copied().flatten() {
                            Some(declared)
                        } else if value.get_type(ctx).deref(ctx).is::<PromiseType>()
                            || value
                                .get_type(ctx)
                                .deref(ctx)
                                .is::<crate::types::OptionalType>()
                        {
                            None
                        } else {
                            match self.analysis.states[id].fact(value) {
                                Fact::Integer(integer) => Some(integer),
                                Fact::Literal { .. } => {
                                    let Range::Bounds(interval) =
                                        self.analysis.states[id].range(value)
                                    else {
                                        return None;
                                    };
                                    let base = self.analysis.states[id]
                                        .required
                                        .get(&value)
                                        .copied()
                                        .unwrap_or(Integer {
                                            bits: if value
                                                .get_type(ctx)
                                                .deref(ctx)
                                                .is::<crate::types::BigIntType>()
                                            {
                                                64
                                            } else {
                                                32
                                            },
                                            signed: true,
                                        });
                                    interval.include(base)
                                }
                                _ => None,
                            }
                        }
                    })
                    .collect();
                let numeric_args = operation
                    .operands()
                    .enumerate()
                    .map(|(position, value)| {
                        let fact = self.analysis.states[id].fact(value);
                        asynchronous.is_none()
                            && declared_args.get(position).copied().flatten().is_none()
                            && !value.get_type(ctx).deref(ctx).is::<PromiseType>()
                            && (fact == Fact::Numeric
                                || (value
                                    .get_type(ctx)
                                    .deref(ctx)
                                    .is::<crate::types::OptionalType>()
                                    && fact.needs_lossless_64()))
                    })
                    .collect();
                let child = self.context(
                    ctx,
                    Key {
                        function: callee.clone(),
                        args,
                        numeric_args,
                        results: operation
                            .results()
                            .enumerate()
                            .map(|(position, value)| {
                                if preserve_result {
                                    ResultDomain::Original
                                } else if let Some(declared) =
                                    declared_results.get(position).copied().flatten()
                                {
                                    ResultDomain::Integer(declared)
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
                if let Some(prev) = self.analysis.states[id].calls.insert(op, child)
                    && prev != child
                {
                    self.analysis.states[prev].callers.remove(&(id, index));
                }
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op);
                let mut call_constrained = false;
                for (position, value) in operation.results().enumerate() {
                    let is_cons = !preserve_result
                        && (declared_results.get(position).copied().flatten().is_some()
                            || self.analysis.states[id].is_constrained(value));
                    if is_cons {
                        call_constrained = true;
                        // Indexed: `constrain` needs `&mut self` while the callee body is read.
                        for ret in 0..self.analysis.bodies[&callee].returns.len() {
                            let ret_op = self.analysis.bodies[&callee].returns[ret];
                            if let Some(ret_val) = ret_op.deref(ctx).operands().nth(position) {
                                self.constrain(child, ret_val, true);
                            }
                        }
                    }
                }
                for (position, input) in operation.operands().enumerate() {
                    let parameter = parameters[position];
                    let required = declared_args.get(position).copied().flatten().or_else(|| {
                        self.analysis.states[child]
                            .required
                            .get(&parameter)
                            .copied()
                    });
                    self.require(id, input, required);
                    let is_cons = declared_args.get(position).copied().flatten().is_some()
                        || (call_constrained
                            && self.analysis.states[child].is_constrained(parameter));
                    self.constrain(id, input, is_cons);
                }
                self.call_results(ctx, id, child, op, asynchronous.is_some());
                return;
            }
        }
        if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx)
            && (matches!(
                binary.kind(ctx),
                BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul
            ) || (matches!(binary.kind(ctx), BinaryKind::Div | BinaryKind::Rem)
                && operation
                    .get_result(0)
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<crate::types::BigIntType>()))
        {
            let result = operation.get_result(0);
            let was_integer = self.analysis.states[id].fact(result).integer().is_some();
            let required = self.analysis.states[id].required.get(&result).copied();
            let mut operand_required = required;
            if operand_required.is_none()
                && result
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<crate::types::BigIntType>()
            {
                operand_required = self.analysis.states[id]
                    .fact(operation.get_operand(0))
                    .integer()
                    .or_else(|| {
                        self.analysis.states[id]
                            .fact(operation.get_operand(1))
                            .integer()
                    });
                if let Some(integer) = operand_required
                    && self.analysis.keys[id].specialize
                {
                    self.require(id, result, Some(integer));
                }
            }
            let constrained = self.analysis.states[id].is_constrained(result);
            for input in operation.operands() {
                self.require(id, input, operand_required);
                self.constrain(id, input, constrained);
            }
            let mut fact = self.arithmetic(
                ctx,
                id,
                &[operation.get_operand(0), operation.get_operand(1)],
                result,
            );
            let range = arithmetic_range(
                binary.kind(ctx),
                self.analysis.states[id].range(operation.get_operand(0)),
                self.analysis.states[id].range(operation.get_operand(1)),
                fact.integer(),
            );
            if let Fact::Literal { negative } = &mut fact {
                *negative |= !matches!(range, Range::Bounds(i) if i.lower >= 0);
            }
            self.fact(id, result, fact);
            self.range(id, result, range);
            if !was_integer
                && self.analysis.keys[id].specialize
                && required.is_none()
                && !result.get_type(ctx).deref(ctx).is::<IntegerType>()
                && (self.analysis.states[id].fact(result).integer().is_some()
                    || result
                        .get_type(ctx)
                        .deref(ctx)
                        .is::<crate::types::BigIntType>())
            {
                self.width_defaults.push((id, index, result));
            }
            return;
        }
        if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx)
            && binary.kind(ctx) == BinaryKind::Rem
        {
            let result = operation.get_result(0);
            let lhs = operation.get_operand(0);
            let rhs = operation.get_operand(1);
            let fact = if self.analysis.keys[id].specialize
                && (self.analysis.states[id].fact(lhs).needs_lossless_64()
                    || self.analysis.states[id].fact(rhs).needs_lossless_64())
            {
                Fact::Numeric
            } else {
                Fact::Original
            };
            let range = arithmetic_range(
                BinaryKind::Rem,
                self.analysis.states[id].range(lhs),
                self.analysis.states[id].range(rhs),
                None,
            );
            self.fact(id, result, fact);
            self.range(id, result, range);
            return;
        }
        if Operation::get_op::<IntrinsicOp>(op, ctx)
            .is_some_and(|op| op.intrinsic(ctx) == Some(Intrinsic::BigIntNegate))
        {
            let required = self.analysis.states[id]
                .required
                .get(&operation.get_result(0))
                .copied();
            let constrained = self.analysis.states[id].is_constrained(operation.get_result(0));
            self.require(id, operation.get_operand(0), required);
            self.constrain(id, operation.get_operand(0), constrained);
            let mut fact = self.arithmetic(
                ctx,
                id,
                &[operation.get_operand(0)],
                operation.get_result(0),
            );
            let range = arithmetic_range(
                BinaryKind::Sub,
                Range::Bounds(Interval { lower: 0, upper: 0 }),
                self.analysis.states[id].range(operation.get_operand(0)),
                fact.integer(),
            );
            if let Fact::Literal { negative } = &mut fact {
                *negative |= !matches!(range, Range::Bounds(i) if i.lower >= 0);
            }
            self.fact(id, operation.get_result(0), fact);
            self.range(id, operation.get_result(0), range);
            return;
        }
        if let Some(intrinsic_op) = Operation::get_op::<IntrinsicOp>(op, ctx)
            && let Some(intrinsic) = intrinsic_op.intrinsic(ctx)
            && let Some(contract) = intrinsic.numeric_contract()
        {
            let result = operation.get_result(0);
            self.outcome(id, result, Outcomes::unknown(ctx, result.get_type(ctx)));
            match contract {
                BuiltinContract::Round => {
                    let input = operation.get_operand(0);
                    let required = self.analysis.states[id].required.get(&result).copied();
                    let input_fact = self.analysis.states[id].fact(input);
                    let fact = if let Some(integer) = input_fact.integer() {
                        Fact::Integer(integer)
                    } else if let Some(req) = required {
                        Fact::Integer(req)
                    } else if self.analysis.keys[id].specialize && input_fact == Fact::Numeric {
                        Fact::Numeric
                    } else {
                        Fact::Original
                    };
                    let range = match self.analysis.states[id].range(input) {
                        Range::Bounds(interval) => Range::Bounds(interval),
                        Range::Bottom => Range::Bottom,
                        Range::Unknown => fact
                            .integer()
                            .map_or(Range::Unknown, |integer| Range::Bounds(integer.bounds())),
                    };
                    self.fact(id, result, fact);
                    self.range(id, result, range);
                    return;
                }
                BuiltinContract::Abs => {
                    let input = operation.get_operand(0);
                    let required = self.analysis.states[id].required.get(&result).copied();
                    let constrained = self.analysis.states[id].is_constrained(result);
                    self.require(id, input, required);
                    self.constrain(id, input, constrained);
                    let fact = self.arithmetic(ctx, id, &[input], result);
                    let range = match self.analysis.states[id].range(input) {
                        Range::Bottom => Range::Bottom,
                        Range::Bounds(interval) => {
                            let lower = if interval.lower <= 0 && interval.upper >= 0 {
                                0
                            } else {
                                interval.lower.abs().min(interval.upper.abs())
                            };
                            let upper = interval.lower.abs().max(interval.upper.abs());
                            let candidate = Interval { lower, upper };
                            if let Some(domain) = fact.integer()
                                && !candidate.fits(domain)
                            {
                                Range::Bounds(domain.bounds())
                            } else {
                                Range::Bounds(candidate)
                            }
                        }
                        Range::Unknown => fact
                            .integer()
                            .map_or(Range::Unknown, |integer| Range::Bounds(integer.bounds())),
                    };
                    self.fact(id, result, fact);
                    self.range(id, result, range);
                    return;
                }
                BuiltinContract::Min | BuiltinContract::Max => {
                    let lhs = operation.get_operand(0);
                    let rhs = operation.get_operand(1);
                    let required = self.analysis.states[id].required.get(&result).copied();
                    let constrained = self.analysis.states[id].is_constrained(result);
                    self.require(id, lhs, required);
                    self.require(id, rhs, required);
                    self.constrain(id, lhs, constrained);
                    self.constrain(id, rhs, constrained);
                    let fact = self.arithmetic(ctx, id, &[lhs, rhs], result);
                    let range = match (
                        self.analysis.states[id].range(lhs),
                        self.analysis.states[id].range(rhs),
                    ) {
                        (Range::Bottom, _) | (_, Range::Bottom) => Range::Bottom,
                        (Range::Bounds(a), Range::Bounds(b)) => {
                            let candidate = if matches!(contract, BuiltinContract::Min) {
                                Interval {
                                    lower: a.lower.min(b.lower),
                                    upper: a.upper.min(b.upper),
                                }
                            } else {
                                Interval {
                                    lower: a.lower.max(b.lower),
                                    upper: a.upper.max(b.upper),
                                }
                            };
                            if let Some(domain) = fact.integer()
                                && !candidate.fits(domain)
                            {
                                Range::Bounds(domain.bounds())
                            } else {
                                Range::Bounds(candidate)
                            }
                        }
                        _ => fact
                            .integer()
                            .map_or(Range::Unknown, |integer| Range::Bounds(integer.bounds())),
                    };
                    self.fact(id, result, fact);
                    self.range(id, result, range);
                    return;
                }
                BuiltinContract::UnsignedCount => {
                    let u32_int = Integer {
                        bits: 32,
                        signed: false,
                    };
                    self.fact(id, result, Fact::Integer(u32_int));
                    self.range(
                        id,
                        result,
                        Range::Bounds(Interval {
                            lower: 0,
                            upper: u32::MAX as i128,
                        }),
                    );
                    return;
                }
                BuiltinContract::ByteValue => {
                    let u8_int = Integer {
                        bits: 8,
                        signed: false,
                    };
                    self.fact(id, result, Fact::Integer(u8_int));
                    self.range(
                        id,
                        result,
                        Range::Bounds(Interval {
                            lower: 0,
                            upper: u8::MAX as i128,
                        }),
                    );
                    return;
                }
                BuiltinContract::NumberOnly => {
                    if intrinsic == Intrinsic::ParseInt {
                        match super::domains::const_parsed_int(ctx, op) {
                            Some(super::domains::ConstParsedInt::Integer {
                                interval,
                                needs_i64,
                                ..
                            }) => {
                                let can_specialize = self.analysis.keys[id].specialize
                                    && !crate::calls::is_async(
                                        ctx,
                                        &self.analysis.bodies[&self.analysis.keys[id].function]
                                            .function,
                                    );
                                let fact = if needs_i64 {
                                    if can_specialize {
                                        Fact::Numeric
                                    } else {
                                        Fact::Original
                                    }
                                } else {
                                    Fact::Literal {
                                        negative: interval.lower < 0,
                                    }
                                };
                                self.fact(id, result, fact);
                                self.range(id, result, Range::Bounds(interval));
                                return;
                            }
                            Some(super::domains::ConstParsedInt::Float(_)) => {
                                self.fact(id, result, Fact::Original);
                                self.range(id, result, Range::Unknown);
                                return;
                            }
                            None => {
                                let fact = if self.analysis.keys[id].specialize
                                    && !crate::calls::is_async(
                                        ctx,
                                        &self.analysis.bodies[&self.analysis.keys[id].function]
                                            .function,
                                    ) {
                                    Fact::Numeric
                                } else {
                                    Fact::Original
                                };
                                self.fact(id, result, fact);
                                self.range(id, result, Range::Unknown);
                                return;
                            }
                        }
                    }
                    self.fact(id, result, Fact::Original);
                    self.range(id, result, Range::Unknown);
                    return;
                }
                BuiltinContract::Predicate => {}
            }
        }
        if Operation::get_op::<CompareOp>(op, ctx).is_some()
            && let Some(domain) = comparison_domain(
                &self.analysis.states[id],
                &[operation.get_operand(0), operation.get_operand(1)],
            )
        {
            for input in operation.operands() {
                if matches!(self.analysis.states[id].fact(input), Fact::Literal { .. }) {
                    self.require(id, input, Some(domain));
                }
            }
        }
        if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
            let operand = operation.get_operand(0);
            let result = operation.get_result(0);
            if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
                self.require(id, operand, Some(integer));
                self.constrain(id, operand, true);
                self.fact(id, result, Fact::Integer(integer));
                let range = match (
                    self.analysis.states[id].fact(operand),
                    self.analysis.states[id].range(operand),
                ) {
                    (Fact::Literal { .. } | Fact::Integer(_), Range::Bounds(interval))
                        if interval.fits(integer) =>
                    {
                        Range::Bounds(interval)
                    }
                    _ => Range::Bounds(integer.bounds()),
                };
                self.range(id, result, range);
                return;
            }
            if let Some(integer) = Integer::from_type(ctx, operand.get_type(ctx)) {
                self.fact(id, result, Fact::Integer(integer));
                self.range(id, result, self.analysis.states[id].range(operand));
                return;
            }
        }
        for result in operation.results() {
            if Operation::get_op::<PromiseRejectedOp>(op, ctx).is_some() {
                self.outcome(id, result, Outcomes::Rejected);
            } else if !matches!(
                heap,
                heap::OperationKind::Read { .. }
                    | heap::OperationKind::Alias { .. }
                    | heap::OperationKind::Combine(_)
            ) {
                self.outcome(id, result, Outcomes::unknown(ctx, result.get_type(ctx)));
            }
            let const_interval = constant_range(ctx, op);
            let fact = if literal(ctx, op).is_some() {
                Fact::Literal {
                    negative: const_interval.is_some_and(|i| i.lower < 0),
                }
            } else if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
                Fact::Integer(integer)
            } else if let Some((fact, _)) = heap_facts {
                fact
            } else {
                Fact::Original
            };
            self.fact(id, result, fact);
            let range = if let Some((_, range)) = heap_facts {
                range
            } else if let Some(range) = const_interval {
                Range::Bounds(
                    self.analysis.states[id]
                        .representation(result)
                        .filter(|&domain| !range.fits(domain))
                        .map_or(range, Integer::bounds),
                )
            } else if Operation::get_op::<crate::collections::StringLenOp>(op, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayLenOp>(op, ctx).is_some()
                || Operation::get_op::<IntrinsicOp>(op, ctx)
                    .is_some_and(|op| op.intrinsic(ctx) == Some(Intrinsic::BytesLength))
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
