use super::{
    ArgumentDomain, Constraint, Demand, Fact, Integer, Interval, Key, Outcomes, Range,
    Representation, ResultDomain, Solver, ValueFacts, Work, arithmetic_range, comparison_domain,
    constant_range, declared, guards::orders, integer_arithmetic, literal,
};
use crate::{
    attrs::BinaryKind,
    calls::AsyncCallOp,
    control::CondBrOp,
    foreign::Phase,
    intrinsics::{BuiltinContract, Intrinsic},
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    semantics::OperationKind,
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

/// Arithmetic may wrap only when every consumer explicitly truncates its result.
fn wrapping_domain(ctx: &Context, result: Value) -> Option<Integer> {
    let mut domain: Option<Integer> = None;
    for usage in result.uses(ctx).iter() {
        let user = usage.user_op();
        Operation::get_op::<NumericCastOp>(user, ctx)?;
        let integer = Integer::from_type(ctx, user.deref(ctx).get_result(0).get_type(ctx))?;
        domain = Some(domain.map_or(integer, |domain| domain.common(integer)));
    }
    domain
}

/// Constant operands can use scalar arithmetic when both inputs and the result fit.
fn constant_bigint_domain(ctx: &Context, kind: BinaryKind, inputs: [Value; 2]) -> Option<Integer> {
    let [Some(a), Some(b)] = inputs.map(|value| constant_range(ctx, value.defining_op()?)) else {
        return None;
    };
    let range = match kind {
        BinaryKind::Div | BinaryKind::Rem => {
            let value = if kind == BinaryKind::Div {
                a.lower.checked_div(b.lower)?
            } else {
                a.lower.checked_rem(b.lower)?
            };
            Interval {
                lower: value,
                upper: value,
            }
        }
        _ => match arithmetic_range(kind, Range::Bounds(a), Range::Bounds(b), None) {
            Range::Bounds(range) => range,
            _ => return None,
        },
    };
    [
        Integer::I64,
        Integer {
            bits: 64,
            signed: false,
        },
    ]
    .into_iter()
    .find(|&domain| [a, b, range].into_iter().all(|range| range.fits(domain)))
}

impl Solver {
    pub(super) fn operation_effects(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        op: Ptr<Operation>,
        kind: OperationKind,
    ) {
        let operation = op.deref(ctx);
        if kind == OperationKind::Await
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
        let escaping = match kind {
            OperationKind::Foreign(Phase::Start)
            | OperationKind::Return
            | OperationKind::Raise
            | OperationKind::Call { asynchronous: true } => 0..operation.get_num_operands(),
            OperationKind::Intrinsic(Intrinsic::ExceptionSetCause) => 1..2,
            OperationKind::Mutate(mutation) => mutation
                .stored_operand()
                .map_or(0..0, |index| index..index + 1),
            _ => 0..0,
        };
        for index in escaping {
            self.escape(id, operation.get_operand(index));
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
                        kind == OperationKind::Call {
                            asynchronous: false,
                        },
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

    pub(super) fn update(&mut self, id: usize, value: Value, incoming: ValueFacts) {
        let state = &mut self.analysis.states[id];
        let (changed, widened) = state
            .values
            .entry(value)
            .or_default()
            .update(incoming, self.analysis.keys[id].specialize);
        self.analysis.range_widenings += usize::from(widened);
        if changed {
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
            let mut facts = self.analysis.states[child].results[position].facts;
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
            facts.outcome = outcome;
            self.update(id, result, facts);
        }
    }

    fn require(&mut self, id: usize, value: Value, demand: Demand) {
        if self.analysis.states[id].demand(value, demand) {
            self.changed(id, value);
        }
    }

    fn guarded_arithmetic_ranges(
        &self,
        ctx: &Context,
        id: usize,
        op: Ptr<Operation>,
        inputs: [Value; 2],
    ) -> [Range; 2] {
        let mut ranges = inputs.map(|input| self.analysis.states[id].range(input));
        let Some(origin) = op.deref(ctx).get_parent_block() else {
            return ranges;
        };
        let mut block = origin;
        let order = loop {
            if block.num_preds(ctx) != 1 {
                return ranges;
            }
            let Some(predecessor) = block.preds(ctx).into_iter().next() else {
                return ranges;
            };
            let Some(terminator) = predecessor.deref(ctx).get_terminator(ctx) else {
                return ranges;
            };
            let operation = terminator.deref(ctx);
            if Operation::get_op::<CondBrOp>(terminator, ctx).is_some() {
                let Some(order) = orders(ctx, predecessor)
                    .into_iter()
                    .flatten()
                    .find(|order| order.target == block)
                else {
                    return ranges;
                };
                break order;
            }
            if operation.get_num_successors() != 1 || predecessor == origin {
                return ranges;
            }
            block = predecessor;
        };
        for (input, range) in inputs.into_iter().zip(&mut ranges) {
            if !matches!(range, Range::Bounds(_)) {
                continue;
            }
            let bound = if input == order.smaller {
                order.larger
            } else if input == order.larger {
                order.smaller
            } else {
                continue;
            };
            let Some(bound) = bound.defining_op().and_then(|op| constant_range(ctx, op)) else {
                continue;
            };
            *range = order.constrain(input, *range, bound);
        }
        ranges
    }

    /// Compute the fact and range of integer arithmetic `result` of `inputs`, whose operand
    /// `ranges` combine according to `kind`. A literal remains non-negative while its range stays non-negative.
    fn arithmetic_result(
        &mut self,
        ctx: &Context,
        id: usize,
        kind: BinaryKind,
        inputs: &[Value],
        ranges: [Range; 2],
        result: Value,
    ) -> ValueFacts {
        let mut fact = self.arithmetic(ctx, id, inputs.iter().copied(), result);
        let [lhs, rhs] = ranges;
        let exact = arithmetic_range(kind, lhs, rhs, None);
        if !result.get_type(ctx).deref(ctx).is::<IntegerType>()
            && let Some(integer) = fact.integer()
            && matches!(self.analysis.states[id].usage(result), Demand::Observed(_))
        {
            let fits_register = matches!(exact, Range::Bottom)
                || matches!(exact, Range::Bounds(interval) if interval.fits(integer));
            let may_produce_negative_zero = kind == BinaryKind::Mul
                && matches!((lhs, rhs), (Range::Bounds(a), Range::Bounds(b))
                    if (a.lower < 0 && b.lower <= 0 && b.upper >= 0)
                        || (b.lower < 0 && a.lower <= 0 && a.upper >= 0));
            if may_produce_negative_zero || (!fits_register && !self.can_specialize(ctx, id)) {
                if inputs.iter().any(|&input| {
                    let facts = self.analysis.states[id].value(input);
                    facts.domain == Fact::Numeric
                        || (facts.domain.needs_lossless_64() && !facts.range.fits_exact_number())
                }) {
                    fact = Fact::Numeric;
                } else {
                    self.require(id, result, Demand::Original);
                    fact = Fact::Original;
                }
            } else if !fits_register {
                let widened = match exact {
                    Range::Bounds(interval) => interval.include(integer),
                    Range::Bottom | Range::Unknown => None,
                };
                fact = Fact::Integer(widened.unwrap_or_else(|| integer.unconstrained()));
            }
        }
        let range = arithmetic_range(kind, lhs, rhs, fact.integer());
        if let Fact::Literal { negative } = &mut fact {
            *negative |= !matches!(range, Range::Bounds(i) if i.lower >= 0);
        }
        ValueFacts::numeric(fact, range)
    }

    /// Compute the fact and range of the result of `op` for a service with a numeric `contract`.
    /// Returns `None` for predicates whose boolean results are ordinary values.
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
                let fact = match (state.fact(input), state.width(result)) {
                    (Fact::Integer(integer), _) | (_, Some(integer)) => Fact::Integer(integer),
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
                let required = self.analysis.states[id].usage(result).width();
                for input in inputs.clone() {
                    self.require(id, input, Demand::Observed(required));
                }
                let mut fact = self.arithmetic(ctx, id, inputs.clone(), result);
                if matches!(self.analysis.states[id].usage(result), Demand::Observed(_))
                    && fact.integer().is_some_and(|integer| integer.bits <= 32)
                {
                    fact = Fact::Original;
                }
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
                    Some(Range::Bounds(interval)) if contract == BuiltinContract::Abs => interval
                        .checked_abs()
                        .map_or(Range::of(fact.integer()), |interval| {
                            Range::clamp(interval, fact.integer())
                        }),
                    Some(Range::Bounds(interval)) => Range::clamp(interval, fact.integer()),
                    None | Some(Range::Unknown) => Range::of(fact.integer()),
                };
                (fact, range)
            }
            BuiltinContract::UnsignedCount
            | BuiltinContract::ByteValue
            | BuiltinContract::Signed32 => {
                if intrinsic.int32_operands() {
                    // Only the low 32 bits of each operand reach the result.
                    for input in inputs {
                        let unsigned = self.analysis.states[id]
                            .fact(input)
                            .integer()
                            .is_some_and(|integer| !integer.signed);
                        let required = if unsigned { Integer::U32 } else { Integer::I32 };
                        self.require(id, input, Demand::Truncated(Some(required)));
                    }
                }
                let integer = match contract {
                    BuiltinContract::ByteValue => Integer::U8,
                    BuiltinContract::Signed32 => Integer::I32,
                    _ => Integer::U32,
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
                        interval.include(state.width(value).unwrap_or(base)).into()
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
                    None if matches!(state.usage(value), Demand::Observed(_)) => {
                        ResultDomain::Inferred
                    }
                    None => state.width(value).into(),
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

    /// Check whether context `id` may assign lossless 64-bit numeric representations to values.
    /// Async functions retain their promise payload representations.
    fn can_specialize(&self, ctx: &Context, id: usize) -> bool {
        let key = &self.analysis.keys[id];
        key.specialize
            && !crate::calls::is_async(ctx, &self.analysis.bodies[&key.function].function)
    }

    /// Aliases carry the same requirements back to their sources.
    fn flow(&mut self, id: usize, result: Value, input: Value) {
        self.require(id, input, self.analysis.states[id].usage(result));
    }

    pub(super) fn process(&mut self, ctx: &Context, id: usize, index: usize) -> Result<()> {
        let constraint = self.analysis.bodies[&self.analysis.keys[id].function].constraints[index];
        let (op, kind) = match constraint {
            Constraint::Edge { from, to } => {
                self.heap_alias(ctx, id, from, id, to, Work::Operation(id, index))?;
                self.propagate_origins(id, from, to);
                // Joined promises share one payload representation, and no cast converts a
                // promise; unequal payload widths keep the original number payload.
                let state = &self.analysis.states[id];
                let (source, target) = (state.fact(from), state.fact(to));
                if from.get_type(ctx).deref(ctx).is::<PromiseType>()
                    && (state.preserves_original(from)
                        || state.preserves_original(to)
                        || source == Fact::Original
                        || target == Fact::Original
                        || source
                            .integer()
                            .zip(target.integer())
                            .is_some_and(|(source, target)| source != target))
                {
                    self.require(id, from, Demand::Original);
                    self.require(id, to, Demand::Original);
                }
                self.flow(id, to, from);
                self.update(id, to, self.analysis.states[id].value(from));
                return Ok(());
            }
            Constraint::Op { op, kind } => (op, kind),
        };
        self.operation_effects(ctx, id, index, op, kind);
        let heap_facts = self.heap_operation(ctx, id, index, op, kind)?;
        let operation = op.deref(ctx);
        let dynamic = Operation::get_op_dyn(op, ctx);
        if kind != OperationKind::Await && op_cast::<dyn BranchOpInterface>(&*dynamic).is_none() {
            for value in operation.operands() {
                if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    self.require(id, value, Demand::Original);
                }
            }
        }
        if let Some(branch) = op_cast::<dyn BranchOpInterface>(&*dynamic) {
            for (position, input) in operation.operands().enumerate() {
                if !(0..operation.get_num_successors()).any(|successor| {
                    branch
                        .successor_operand_range(ctx, successor)
                        .contains(&position)
                }) {
                    self.require(id, input, Demand::Observed(None));
                }
            }
        } else if kind != OperationKind::Return
            && crate::calls::callee(ctx, op).is_none()
            && integer_arithmetic(ctx, op).is_none()
            && kind != OperationKind::NumericCast
            && kind != OperationKind::Await
            && kind.promise_payload().is_none()
            && !matches!(kind, OperationKind::Intrinsic(intrinsic) if intrinsic.int32_operands())
        {
            for input in operation.operands() {
                self.require(id, input, Demand::Observed(None));
            }
        }
        if kind == OperationKind::Return {
            let mut changed = false;
            let is_summary = self.is_summary(id);
            for (position, value) in operation.operands().enumerate() {
                let key = &self.analysis.keys[id];
                let declared = declared(
                    &self.analysis.bodies[&key.function].declared_results,
                    position,
                )
                .filter(|_| {
                    !value
                        .get_type(ctx)
                        .deref(ctx)
                        .is::<crate::types::BigIntType>()
                });
                let required = key.results[position];
                let demand = match required {
                    ResultDomain::Original => Demand::Observed(None),
                    ResultDomain::Integer(integer) => Demand::Truncated(Some(integer)),
                    ResultDomain::Inferred => match declared {
                        Some(integer) => Demand::Truncated(Some(integer)),
                        None if self.analysis.bodies[&key.function].external => {
                            Demand::Observed(None)
                        }
                        None if is_summary => Demand::Truncated(None),
                        None => Demand::None,
                    },
                };
                self.require(id, value, demand);
                let fact = match (required, declared, self.analysis.states[id].fact(value)) {
                    (ResultDomain::Original, ..) => Fact::Original,
                    (_, Some(declared), _) => Fact::Integer(declared),
                    (ResultDomain::Integer(req), None, fact @ Fact::Literal { .. }) => {
                        fact.join(Fact::Integer(req))
                    }
                    (_, None, fact) => fact,
                };
                let mut incoming = self.analysis.states[id].value(value);
                incoming.domain = fact;
                let representation = Representation::value(
                    self.analysis.keys[id].specialize && required != ResultDomain::Original,
                    None,
                );
                let (updated, widened) = self.analysis.states[id].results[position]
                    .facts
                    .join(incoming, representation);
                changed |= updated;
                self.analysis.range_widenings += usize::from(widened);
            }
            if changed {
                self.notify_callers(id);
            }
            return Ok(());
        }
        let asynchronous = kind == OperationKind::Call { asynchronous: true };
        if let Some(callee) = crate::calls::callee(ctx, op) {
            if !self.analysis.keys[id].specialize
                && let Some(child) = self.analysis.generic_context(&callee)
            {
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op)?;
                self.call_results(ctx, id, child, op, asynchronous);
                return Ok(());
            }
            if operation
                .operands()
                .any(|value| self.analysis.states[id].fact(value) == Fact::Bottom)
            {
                return Ok(());
            }
            if self.analysis.bodies.contains_key(&callee) {
                let preserve_result = asynchronous
                    && self.analysis.states[id].preserves_original(operation.get_result(0));
                let key = self.call_key(ctx, id, op, &callee, preserve_result);
                let child = self.context(ctx, key)?;
                if let Some(prev) = self.analysis.states[id].calls.insert(op, child)
                    && prev != child
                {
                    self.analysis.states[prev].callers.remove(&(id, index));
                }
                self.analysis.states[child].callers.insert((id, index));
                self.heap_call(ctx, id, index, child, op)?;
                for (position, value) in operation.results().enumerate() {
                    if declared(&self.analysis.bodies[&callee].declared_results, position).is_none()
                    {
                        let demand = self.analysis.states[id].usage(value);
                        for ret in 0..self.analysis.bodies[&callee].returns.len() {
                            let ret_op = self.analysis.bodies[&callee].returns[ret];
                            if let Some(ret_val) = ret_op.deref(ctx).operands().nth(position) {
                                self.require(child, ret_val, demand);
                            }
                        }
                    }
                }
                for (position, input) in operation.operands().enumerate() {
                    let body = &self.analysis.bodies[&callee];
                    let demand = match declared(&body.declared_args, position) {
                        Some(integer) => Demand::Truncated(Some(integer)),
                        None => self.analysis.states[child].usage(body.parameters[position]),
                    };
                    self.require(id, input, demand);
                }
                self.call_results(ctx, id, child, op, asynchronous);
                return Ok(());
            }
        }
        if Operation::get_op::<CompareOp>(op, ctx).is_some()
            && let Some(domain) = comparison_domain(
                ctx,
                &self.analysis.states[id],
                &[operation.get_operand(0), operation.get_operand(1)],
            )
        {
            for input in operation.operands() {
                if matches!(self.analysis.states[id].fact(input), Fact::Literal { .. }) {
                    self.require(id, input, Demand::Observed(Some(domain)));
                }
            }
        }
        for result in operation.results() {
            let facts = self.result_facts(ctx, id, op, result, (kind, heap_facts));
            self.update(id, result, facts);
        }
        Ok(())
    }

    fn result_facts(
        &mut self,
        ctx: &Context,
        id: usize,
        op: Ptr<Operation>,
        result: Value,
        (kind, heap_facts): (OperationKind, Option<ValueFacts>),
    ) -> ValueFacts {
        let operation = op.deref(ctx);
        if kind == OperationKind::Await || kind.promise_payload().is_some() {
            let input = operation.get_operand(kind.promise_payload().unwrap_or(0));
            let promise = if kind == OperationKind::Await {
                input
            } else {
                result
            };
            let mut facts = self.analysis.states[id].value(input);
            facts.outcome = match kind {
                OperationKind::Await => Outcomes::unknown(ctx, result.get_type(ctx)),
                OperationKind::Resolved => Outcomes::Fulfilled,
                _ => Outcomes::Either,
            };
            if kind == OperationKind::Await && facts.domain == Fact::Original {
                self.require(id, promise, Demand::Original);
            }
            if self.analysis.states[id].preserves_original(promise) {
                if kind != OperationKind::Await {
                    facts.domain = Fact::Original;
                }
            } else {
                self.flow(id, result, input);
            }
            return facts;
        }
        if let Some(kind) = integer_arithmetic(ctx, op) {
            let (lhs, rhs) = (operation.get_operand(0), operation.get_operand(1));
            let bigint = result
                .get_type(ctx)
                .deref(ctx)
                .is::<crate::types::BigIntType>();
            if bigint {
                // A WIT input bounds its value, not the result of arbitrary-precision arithmetic.
                let domain = self.analysis.keys[id]
                    .specialize
                    .then(|| {
                        constant_bigint_domain(ctx, kind, [lhs, rhs]).or_else(|| {
                            matches!(kind, BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul)
                                .then(|| wrapping_domain(ctx, result))
                                .flatten()
                        })
                    })
                    .flatten();
                return ValueFacts::numeric(
                    domain.map_or(Fact::Original, Fact::Integer),
                    Range::of(domain),
                );
            }
            let width = self.analysis.states[id].usage(result).width();
            for input in [lhs, rhs] {
                self.require(id, input, Demand::Observed(width));
            }
            let ranges = self.guarded_arithmetic_ranges(ctx, id, op, [lhs, rhs]);
            return self.arithmetic_result(ctx, id, kind, &[lhs, rhs], ranges, result);
        }
        if let Some(binary) = Operation::get_op::<BinaryOp>(op, ctx)
            && binary.kind(ctx) == BinaryKind::Rem
        {
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
            return ValueFacts::numeric(fact, range);
        }
        if kind == OperationKind::Intrinsic(Intrinsic::BigIntNegate) {
            let domain = self.analysis.keys[id]
                .specialize
                .then(|| wrapping_domain(ctx, result))
                .flatten();
            return ValueFacts::numeric(
                domain.map_or(Fact::Original, Fact::Integer),
                Range::of(domain),
            );
        }
        if let OperationKind::Intrinsic(intrinsic) = kind
            && let Some(contract) = intrinsic.policy().numeric
            && let Some((fact, range)) = self.contract_result(ctx, id, op, intrinsic, contract)
        {
            let mut facts = ValueFacts::numeric(fact, range);
            facts.outcome = Outcomes::unknown(ctx, result.get_type(ctx));
            return facts;
        }
        if kind == OperationKind::NumericCast {
            let operand = operation.get_operand(0);
            if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
                self.require(id, operand, Demand::Truncated(Some(integer)));
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
                return ValueFacts::numeric(Fact::Integer(integer), range);
            }
            if let Some(integer) = Integer::from_type(ctx, operand.get_type(ctx)) {
                return ValueFacts::numeric(
                    Fact::Integer(integer),
                    self.analysis.states[id].range(operand),
                );
            }
        }
        let outcome = if kind == OperationKind::Rejected {
            Outcomes::Rejected
        } else if let Some(facts) = heap_facts {
            facts.outcome
        } else if matches!(kind, OperationKind::Combine(_)) {
            Outcomes::Bottom
        } else {
            Outcomes::unknown(ctx, result.get_type(ctx))
        };
        let const_interval = constant_range(ctx, op);
        let fact = if literal(ctx, op).is_some() {
            Fact::Literal {
                negative: const_interval.is_some_and(|i| i.lower < 0),
            }
        } else if let Some(integer) = Integer::from_type(ctx, result.get_type(ctx)) {
            Fact::Integer(integer)
        } else if let Some(facts) = heap_facts {
            facts.domain
        } else {
            Fact::Original
        };
        let range = if let Some(facts) = heap_facts {
            facts.range
        } else if let Some(range) = const_interval {
            // A literal keeps its exact value; a demanded width it does not fit leaves it
            // unspecialized rather than clamped.
            Range::clamp(range, fact.integer())
        } else if kind == OperationKind::Length
            || kind == OperationKind::Intrinsic(Intrinsic::BytesLength)
        {
            Range::of(Some(Integer::U32))
        } else if fact.integer().is_some() {
            Range::of(fact.integer())
        } else if kind == OperationKind::NumericCast {
            self.analysis.states[id].range(operation.get_operand(0))
        } else {
            Range::Unknown
        };
        let mut facts = ValueFacts::numeric(fact, range);
        facts.outcome = outcome;
        facts
    }
}
