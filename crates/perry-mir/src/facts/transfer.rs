use super::{
    ArgumentDomain, Constraint, Fact, Integer, Interval, Key, Outcomes, Range, ResultDomain,
    Solver, Work, arithmetic_range, comparison_domain, constant_range, declared, heap,
    integer_arithmetic, literal, promise_payload,
};
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    collections::{ArrayExtendOp, ArrayPushOp, ArraySetOp},
    control::ReturnOp,
    heap::FieldSetOp,
    intrinsics::{BuiltinContract, Intrinsic, PromiseRejectedOp, PromiseResolvedOp, intrinsic_of},
    locals::StoreOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{op_interfaces::BranchOpInterface, types::IntegerType},
    context::{Context, Ptr},
    identifier::Identifier,
    op::op_cast,
    operation::Operation,
    result::Result,
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
        let started = Operation::get_op::<crate::foreign::ForeignOp>(op, ctx)
            .is_some_and(|op| op.phase(ctx) == crate::foreign::Phase::Start);
        if started || crate::is_any::<(ReturnOp, crate::exceptions::RaiseOp, AsyncCallOp)>(ctx, op)
        {
            for value in operation.operands() {
                self.escape(id, value);
            }
        }
        // The operand a write stores, which escapes into memory.
        let stored = if intrinsic_of(ctx, op) == Some(Intrinsic::ExceptionSetCause)
            || crate::is_any::<(StoreOp, FieldSetOp, ArrayPushOp, ArrayExtendOp)>(ctx, op)
        {
            Some(1)
        } else {
            Operation::is_op::<ArraySetOp>(op, ctx).then_some(2)
        };
        if let Some(stored) = stored {
            self.escape(id, operation.get_operand(stored));
        }
        if let Some(callee) = crate::calls::callee(ctx, op) {
            if let Some(child) = self.analysis.generic_context(&callee) {
                self.analysis.states[child].callers.insert((id, index));
                let current = self.analysis.states[id].summary.effects;
                let effects = self.analysis.bodies[&self.analysis.keys[id].function]
                    .effects
                    .inherit(
                        current,
                        self.analysis.states[child].summary.effects,
                        op,
                        Operation::get_op::<CallOp>(op, ctx).is_some(),
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

    /// Record the fact and range of integer arithmetic `result` of `inputs`, whose operand
    /// `ranges` combine by `kind`; a literal stays non-negative only while its range does.
    fn arithmetic_result(
        &mut self,
        ctx: &Context,
        id: usize,
        kind: BinaryKind,
        inputs: &[Value],
        ranges: [Range; 2],
        result: Value,
    ) {
        let mut fact = self.arithmetic(ctx, id, inputs.iter().copied(), result);
        let [lhs, rhs] = ranges;
        let range = arithmetic_range(kind, lhs, rhs, fact.integer());
        if let Fact::Literal { negative } = &mut fact {
            *negative |= !matches!(range, Range::Bounds(i) if i.lower >= 0);
        }
        self.fact(id, result, fact);
        self.range(id, result, range);
    }

    /// The fact and range of the result of `op`, a service with a numeric `contract`; `None`
    /// for predicates, whose boolean results are ordinary values.
    fn contract_result(
        &mut self,
        ctx: &Context,
        id: usize,
        op: Ptr<Operation>,
        intrinsic: Intrinsic,
        contract: BuiltinContract,
    ) -> Option<(Fact, Range)> {
        let operation = op.deref(ctx);
        let result = operation.get_result(0);
        let inputs = operation.operands();
        let range = |solver: &Self, input: Value| solver.analysis.states[id].range(input);
        Some(match contract {
            BuiltinContract::Round => {
                let input = operation.get_operand(0);
                let state = &self.analysis.states[id];
                let fact = match (state.fact(input), state.required.get(&result)) {
                    (Fact::Integer(integer), _) | (_, Some(&integer)) => Fact::Integer(integer),
                    (Fact::Numeric, None) if self.analysis.keys[id].specialize => Fact::Numeric,
                    _ => Fact::Original,
                };
                let range = match range(self, input) {
                    Range::Unknown => Range::of(fact.integer()),
                    known => known,
                };
                (fact, range)
            }
            BuiltinContract::Abs | BuiltinContract::Min | BuiltinContract::Max => {
                let required = self.analysis.states[id].required.get(&result).copied();
                let constrained = self.analysis.states[id].is_constrained(result);
                for input in inputs.clone() {
                    self.require(id, input, required);
                }
                for input in inputs.clone() {
                    self.constrain(id, input, constrained);
                }
                let fact = self.arithmetic(ctx, id, inputs.clone(), result);
                let max = contract == BuiltinContract::Max;
                let range = inputs
                    .map(|input| range(self, input))
                    .reduce(|a, b| match (a, b) {
                        (Range::Bottom, _) | (_, Range::Bottom) => Range::Bottom,
                        (Range::Bounds(a), Range::Bounds(b)) => Range::Bounds(a.extreme(b, max)),
                        _ => Range::Unknown,
                    });
                let range = match range {
                    Some(Range::Bottom) => Range::Bottom,
                    Some(Range::Bounds(interval)) if contract == BuiltinContract::Abs => {
                        Range::clamp(interval.abs(), fact.integer())
                    }
                    Some(Range::Bounds(interval)) => Range::clamp(interval, fact.integer()),
                    None | Some(Range::Unknown) => Range::of(fact.integer()),
                };
                (fact, range)
            }
            BuiltinContract::UnsignedCount | BuiltinContract::ByteValue => {
                let integer = if contract == BuiltinContract::ByteValue {
                    Integer::U8
                } else {
                    Integer::U32
                };
                (Fact::Integer(integer), Range::of(Some(integer)))
            }
            BuiltinContract::Predicate => return None,
            BuiltinContract::NumberOnly => {
                let numeric = if self.can_specialize(ctx, id) {
                    Fact::Numeric
                } else {
                    Fact::Original
                };
                let parsed = (intrinsic == Intrinsic::ParseInt)
                    .then(|| super::domains::const_parsed_int(ctx, op));
                match parsed {
                    Some(Some(super::domains::ConstParsedInt::Integer {
                        interval,
                        needs_i64,
                        ..
                    })) => {
                        let fact = if needs_i64 {
                            numeric
                        } else {
                            Fact::Literal {
                                negative: interval.lower < 0,
                            }
                        };
                        (fact, Range::Bounds(interval))
                    }
                    Some(None) => (numeric, Range::Unknown),
                    _ => (Fact::Original, Range::Unknown),
                }
            }
        })
    }

    /// The context a specialized call `op` of `callee` needs: declared or known integer argument
    /// domains, numeric arguments, and the result widths its uses require.
    fn call_key(
        &self,
        ctx: &Context,
        id: usize,
        op: Ptr<Operation>,
        callee: &Identifier,
        preserve_result: bool,
    ) -> Key {
        let state = &self.analysis.states[id];
        let body = &self.analysis.bodies[callee];
        let asynchronous = Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
        let operation = op.deref(ctx);
        let args = operation
            .operands()
            .enumerate()
            .map(|(position, value)| {
                let ty = value.get_type(ctx);
                let ty = ty.deref(ctx);
                if let Some(declared) = declared(&body.declared_args, position) {
                    return ArgumentDomain::Integer(declared);
                }
                let fact = state.fact(value);
                if !asynchronous
                    && !ty.is::<PromiseType>()
                    && (fact == Fact::Numeric
                        || (ty.is::<crate::types::OptionalType>() && fact.needs_lossless_64()))
                {
                    return ArgumentDomain::Numeric;
                }
                if ty.is::<PromiseType>() || ty.is::<crate::types::OptionalType>() {
                    return ArgumentDomain::Original;
                }
                match fact {
                    Fact::Integer(integer) => ArgumentDomain::Integer(integer),
                    Fact::Literal { .. } => {
                        let Range::Bounds(interval) = state.range(value) else {
                            return ArgumentDomain::Original;
                        };
                        let base = if ty.is::<crate::types::BigIntType>() {
                            Integer::I64
                        } else {
                            Integer::I32
                        };
                        interval
                            .include(state.required.get(&value).copied().unwrap_or(base))
                            .into()
                    }
                    _ => ArgumentDomain::Original,
                }
            })
            .collect();
        let results = operation
            .results()
            .enumerate()
            .map(
                |(position, value)| match declared(&body.declared_results, position) {
                    _ if preserve_result => ResultDomain::Original,
                    Some(declared) => ResultDomain::Integer(declared),
                    None => state.required.get(&value).copied().into(),
                },
            )
            .collect();
        Key {
            function: callee.clone(),
            args,
            results,
            specialize: true,
        }
    }

    /// Whether context `id` may give values numeric (lossless 64-bit) representations; async
    /// functions keep their promise payloads.
    fn can_specialize(&self, ctx: &Context, id: usize) -> bool {
        let key = &self.analysis.keys[id];
        key.specialize
            && !crate::calls::is_async(ctx, &self.analysis.bodies[&key.function].function)
    }

    /// Flow the width required of `result`, and whether it is constrained, back to `input`.
    fn flow(&mut self, id: usize, result: Value, input: Value) {
        let required = self.analysis.states[id].required.get(&result).copied();
        let constrained = self.analysis.states[id].is_constrained(result);
        self.require(id, input, required);
        self.constrain(id, input, constrained);
    }

    fn preserve_promise_payload(&mut self, id: usize, value: Value) {
        if self.analysis.states[id].original_payloads.insert(value) {
            self.fact(id, value, Fact::Original);
            self.changed(id, value);
        }
    }

    pub(super) fn process(&mut self, ctx: &Context, id: usize, index: usize) -> Result<()> {
        let constraint = self.analysis.bodies[&self.analysis.keys[id].function].constraints[index];
        let (op, heap) = match constraint {
            Constraint::Edge { from, to } => {
                self.outcome(id, to, self.analysis.states[id].outcome(from));
                self.heap_alias(ctx, id, from, id, to, Work::Operation(id, index))?;
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
                self.flow(id, to, from);
                let fact = self.analysis.states[id].fact(from);
                self.fact(id, to, fact);
                self.range(id, to, self.analysis.states[id].range(from));
                return Ok(());
            }
            Constraint::Op { op, heap } => (op, heap),
        };
        self.operation_effects(ctx, id, index, op);
        let heap_facts = self.heap_operation(ctx, id, index, op, heap)?;
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
                self.flow(id, promise, value);
                self.fact(id, promise, self.analysis.states[id].fact(value));
            }
            self.range(id, promise, self.analysis.states[id].range(value));
            return Ok(());
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
                self.flow(id, result, promise);
            }
            self.fact(id, result, fact);
            self.range(id, result, self.analysis.states[id].range(promise));
            return Ok(());
        }
        if Operation::get_op::<ReturnOp>(op, ctx).is_some() {
            let mut changed = false;
            let is_summary = self.is_summary(id);
            for (position, value) in operation.operands().enumerate() {
                let key = &self.analysis.keys[id];
                let declared = declared(
                    &self.analysis.bodies[&key.function].declared_results,
                    position,
                );
                let required = key.results[position];
                self.require(id, value, required.integer());
                let constrained = is_summary || required.integer().is_some() || declared.is_some();
                self.constrain(id, value, constrained);
                let fact = match (required, declared, self.analysis.states[id].fact(value)) {
                    (ResultDomain::Original, ..) => Fact::Original,
                    (_, Some(declared), _) => Fact::Integer(declared),
                    (ResultDomain::Integer(req), None, fact @ Fact::Literal { .. }) => {
                        fact.join(Fact::Integer(req))
                    }
                    (_, None, fact) => fact,
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
            return Ok(());
        }
        let asynchronous = Operation::get_op::<AsyncCallOp>(op, ctx);
        if let Some(callee) = crate::calls::callee(ctx, op) {
            if !self.analysis.keys[id].specialize
                && let Some(child) = self.analysis.generic_context(&callee)
            {
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op)?;
                self.call_results(ctx, id, child, op, asynchronous.is_some());
                return Ok(());
            }
            if operation
                .operands()
                .any(|value| self.analysis.states[id].fact(value) == Fact::Bottom)
            {
                return Ok(());
            }
            if self.analysis.bodies.contains_key(&callee) {
                let preserve_result = asynchronous.is_some()
                    && self.analysis.states[id]
                        .original_payloads
                        .contains(&operation.get_result(0));
                let key = self.call_key(ctx, id, op, &callee, preserve_result);
                let child = self.context(ctx, key)?;
                if let Some(prev) = self.analysis.states[id].calls.insert(op, child)
                    && prev != child
                {
                    self.analysis.states[prev].callers.remove(&(id, index));
                }
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op)?;
                let mut call_constrained = false;
                for (position, value) in operation.results().enumerate() {
                    let is_cons = !preserve_result
                        && (declared(&self.analysis.bodies[&callee].declared_results, position)
                            .is_some()
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
                    let body = &self.analysis.bodies[&callee];
                    let declared = declared(&body.declared_args, position);
                    let parameter = body.parameters[position];
                    let child_state = &self.analysis.states[child];
                    let required =
                        declared.or_else(|| child_state.required.get(&parameter).copied());
                    let is_cons = declared.is_some()
                        || (call_constrained && child_state.is_constrained(parameter));
                    self.require(id, input, required);
                    self.constrain(id, input, is_cons);
                }
                self.call_results(ctx, id, child, op, asynchronous.is_some());
                return Ok(());
            }
        }
        if let Some(kind) = integer_arithmetic(ctx, op) {
            let result = operation.get_result(0);
            let (lhs, rhs) = (operation.get_operand(0), operation.get_operand(1));
            let bigint = result
                .get_type(ctx)
                .deref(ctx)
                .is::<crate::types::BigIntType>();
            let was_integer = self.analysis.states[id].fact(result).integer().is_some();
            let required = self.analysis.states[id].required.get(&result).copied();
            let mut operand_required = required;
            if operand_required.is_none() && bigint {
                let state = &self.analysis.states[id];
                operand_required = state
                    .fact(lhs)
                    .integer()
                    .or_else(|| state.fact(rhs).integer());
                if let Some(integer) = operand_required
                    && self.analysis.keys[id].specialize
                {
                    self.require(id, result, Some(integer));
                }
            }
            let constrained = self.analysis.states[id].is_constrained(result);
            for input in [lhs, rhs] {
                self.require(id, input, operand_required);
                self.constrain(id, input, constrained);
            }
            let ranges = [lhs, rhs].map(|input| self.analysis.states[id].range(input));
            self.arithmetic_result(ctx, id, kind, &[lhs, rhs], ranges, result);
            if !was_integer
                && self.analysis.keys[id].specialize
                && required.is_none()
                && !result.get_type(ctx).deref(ctx).is::<IntegerType>()
                && (self.analysis.states[id].fact(result).integer().is_some() || bigint)
            {
                self.width_defaults.push((id, index, result));
            }
            return Ok(());
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
            return Ok(());
        }
        if intrinsic_of(ctx, op) == Some(Intrinsic::BigIntNegate) {
            let (input, result) = (operation.get_operand(0), operation.get_result(0));
            self.flow(id, result, input);
            let zero = Range::Bounds(Interval { lower: 0, upper: 0 });
            let ranges = [zero, self.analysis.states[id].range(input)];
            self.arithmetic_result(ctx, id, BinaryKind::Sub, &[input], ranges, result);
            return Ok(());
        }
        if let Some(intrinsic) = intrinsic_of(ctx, op)
            && let Some(contract) = intrinsic.numeric_contract()
            && let Some((fact, range)) = self.contract_result(ctx, id, op, intrinsic, contract)
        {
            let result = operation.get_result(0);
            self.outcome(id, result, Outcomes::unknown(ctx, result.get_type(ctx)));
            self.fact(id, result, fact);
            self.range(id, result, range);
            return Ok(());
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
                return Ok(());
            }
            if let Some(integer) = Integer::from_type(ctx, operand.get_type(ctx)) {
                self.fact(id, result, Fact::Integer(integer));
                self.range(id, result, self.analysis.states[id].range(operand));
                return Ok(());
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
                Range::clamp(range, self.analysis.states[id].representation(result))
            } else if Operation::get_op::<crate::collections::StringLenOp>(op, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayLenOp>(op, ctx).is_some()
                || intrinsic_of(ctx, op) == Some(Intrinsic::BytesLength)
            {
                Range::of(Some(Integer::U32))
            } else if fact.integer().is_some() {
                Range::of(fact.integer())
            } else if Operation::get_op::<NumericCastOp>(op, ctx).is_some() {
                self.analysis.states[id].range(operation.get_operand(0))
            } else {
                Range::Unknown
            };
            self.range(id, result, range);
        }
        Ok(())
    }
}
