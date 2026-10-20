use super::*;
use pliron::value::Value;

/// The emitted body and the facts that affect operand conversion and scalar replacement.
#[derive(PartialEq, Eq, Hash)]
pub(super) struct BodyPlan {
    function: Identifier,
    arguments: Vec<(Option<bool>, Option<TypeHandle>)>,
    pub(super) results: Vec<TypeHandle>,
    pub(super) values: Vec<ValuePlan>,
    result_facts: Vec<(Option<Integer>, Option<TypeHandle>)>,
    compares: Vec<(Option<Integer>, bool)>,
    constrained_bins: Vec<bool>,
    calls: Vec<(usize, Option<bool>, Option<Integer>)>,
}

#[derive(PartialEq, Eq, Hash)]
pub(super) struct ValuePlan {
    pub(super) value: Value,
    pub(super) ty: TypeHandle,
    integer: Option<Integer>,
    layout: Option<TypeHandle>,
}

pub(super) struct ContextPlan {
    pub(super) body: BodyPlan,
    pub(super) arguments: Vec<TypeHandle>,
    pub(super) changed: bool,
}

impl ContextPlan {
    pub(super) fn new(
        ctx: &Context,
        analysis: &ProgramFacts,
        id: usize,
        is_root: bool,
    ) -> Result<Self> {
        let key = &analysis.keys[id];
        let body = &analysis.bodies[&key.function];
        let state = &analysis.states[id];
        let mut changed = false;
        let mut values = Vec::new();
        let mut compares = Vec::new();
        let mut constrained_bins = Vec::new();
        let mut calls = Vec::new();
        for &constraint in &body.constraints {
            let count = match constraint {
                Constraint::Op { op, .. } => op.deref(ctx).get_num_results(),
                Constraint::Edge { .. } => 1,
            };
            for position in 0..count {
                let value = match constraint {
                    Constraint::Op { op, .. } => op.deref(ctx).get_result(position),
                    Constraint::Edge { to, .. } => to,
                };
                let original = value.get_type(ctx);
                let integer = state.representation(value);
                let can_specialize = can_specialize_value(ctx, state, value);
                let layout = analysis.specialized_value_type(ctx, id, value)?;
                let ty = if can_specialize && let Some(integer) = integer {
                    let scalar = integer.ty(ctx);
                    if original.deref(ctx).is::<PromiseType>() {
                        PromiseType::get(ctx, scalar).into()
                    } else {
                        scalar
                    }
                } else if !original.deref(ctx).is::<PromiseType>() {
                    layout.unwrap_or(original)
                } else {
                    original
                };
                changed |= ty != original;
                let integer = (can_specialize || matches!(constraint, Constraint::Edge { .. }))
                    .then_some(integer)
                    .flatten();
                values.push(ValuePlan {
                    value,
                    ty,
                    integer,
                    layout,
                });
            }
            let Constraint::Op { op, .. } = constraint else {
                continue;
            };
            if super::super::integer_arithmetic(ctx, op).is_some() {
                constrained_bins.push(state.is_constrained(op.deref(ctx).get_result(0)));
            }
            if Operation::get_op::<CompareOp>(op, ctx).is_some() {
                let inputs = [op.deref(ctx).get_operand(0), op.deref(ctx).get_operand(1)];
                let domain = comparison_domain(state, &inputs);
                compares.push((
                    domain,
                    inputs
                        .iter()
                        .any(|&v| state.fact(v) == super::super::Fact::Numeric),
                ));
                changed |= domain.is_some_and(|domain| domain.ty(ctx) != inputs[0].get_type(ctx))
                    || inputs.iter().any(|&v| state.fact(v).needs_lossless_64());
            }
            changed |= super::super::domains::const_parsed_int(ctx, op).is_some();
            if let Some(&child) = state.calls.get(&op) {
                let call_res = op.deref(ctx).results().next();
                calls.push((
                    child,
                    call_res.map(|r| state.is_constrained(r)),
                    call_res.and_then(|r| {
                        state
                            .required
                            .get(&r)
                            .copied()
                            .or_else(|| state.representation(r))
                    }),
                ));
            }
        }
        let mut arguments = body.signature.deref(ctx).arg_types();
        let mut argument_keys = Vec::new();
        for (pos, ty) in arguments.iter_mut().enumerate() {
            let integer = key.args[pos].integer();
            let layout = analysis.specialized_value_type(ctx, id, body.parameters[pos])?;
            argument_keys.push((integer.map(|i| i.bits == 64 && !i.signed), layout));
            let new = integer
                .map(|integer| integer.ty(ctx))
                .or(layout)
                .unwrap_or(*ty);
            changed |= new != *ty;
            *ty = new;
        }
        let mut results = body.signature.deref(ctx).res_types();
        let mut result_facts = Vec::new();
        for (pos, ty) in results.iter_mut().enumerate() {
            let promise = ty.deref(ctx).is::<PromiseType>();
            let integer = (!promise && !ty.deref(ctx).is::<crate::types::OptionalType>())
                .then(|| state.results[pos].facts.domain.integer())
                .flatten();
            let layout = analysis.specialized_result_type(ctx, id, pos, *ty)?;
            result_facts.push((integer, layout));
            if promise {
                continue;
            }
            let new = match integer {
                Some(integer) => Some(integer.ty(ctx)),
                _ if !is_root || super::super::heap::aggregate(ctx, *ty) => layout,
                _ => None,
            }
            .unwrap_or(*ty);
            changed |= new != *ty;
            *ty = new;
        }
        Ok(Self {
            body: BodyPlan {
                function: key.function.clone(),
                arguments: argument_keys,
                results,
                result_facts,
                values,
                compares,
                constrained_bins,
                calls,
            },
            arguments,
            changed,
        })
    }
}
