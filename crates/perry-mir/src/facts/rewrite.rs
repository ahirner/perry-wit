//! Representation rewriting after ProgramFacts has converged.
use super::{
    Constraint, Integer, ProgramFacts, SPECIALIZATION, WIT_SIGNATURE, comparison_domain, literal,
    promise_payload,
};
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    control::ReturnOp,
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, IntegerAttr, TypeAttr},
        op_interfaces::{BranchOpInterface, OneResultInterface, SymbolOpInterface},
        ops::{ConstantOp, FuncOp},
        type_interfaces::FunctionTypeInterface,
        types::{FunctionType, IntegerType},
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
    op::{Op, op_cast},
    operation::Operation,
    pass::{AnalysisManager, Pass, PassResult},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
};
use std::collections::{HashMap, HashSet};

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
        let analysis = analyses.get_analysis::<ProgramFacts>(root, ctx)?;
        let specializations = if analysis.roots.is_empty() {
            0
        } else {
            rewrite(ctx, &analysis)
        };
        let previous = crate::pipeline::statistics(root, ctx);
        root.deref_mut(ctx).attributes.set(
            crate::pipeline::STATISTICS.clone(),
            crate::attrs::OptimizationStatistics {
                numeric_contexts: analysis.keys.len() as u64,
                constraint_visits: analysis.constraint_visits as u64,
                summarized_heap_sets: analysis.summarized_heap_sets as u64,
                summarized_contexts: analysis.summarized_contexts as u64,
                range_widenings: analysis.range_widenings as u64,
                specializations: specializations as u64,
                cleanup_rounds: 0,
                parse_stringify_rewrites: previous.parse_stringify_rewrites,
                parse_stringify_missed: previous.parse_stringify_missed,
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

struct Implementation {
    function: FuncOp,
    mapping: IrMapping,
    arg_domains: Vec<Option<Integer>>,
    arguments: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
    primary_context: usize,
}

#[derive(PartialEq, Eq, Hash)]
struct CoalesceKey {
    function: Identifier,
    arg_kinds: Vec<Option<bool>>,
    arg_specs: Vec<Option<TypeHandle>>,
    results: Vec<Option<Integer>>,
    result_specs: Vec<Option<TypeHandle>>,
    values: Vec<Option<Integer>>,
    value_specs: Vec<Option<TypeHandle>>,
    compares: Vec<Option<Integer>>,
    numeric_compares: Vec<bool>,
    constrained_bins: Vec<bool>,
    calls: Vec<(usize, Option<bool>, Option<Integer>)>,
}

fn can_specialize_value(ctx: &Context, state: &super::State, value: pliron::value::Value) -> bool {
    if value
        .get_type(ctx)
        .deref(ctx)
        .is::<crate::types::OptionalType>()
    {
        return false;
    }
    let Some(op) = value.defining_op() else {
        return true;
    };
    if Operation::get_op::<crate::heap::FieldGetOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::ArrayAtOp>(op, ctx).is_some()
        || Operation::get_op::<crate::locals::LoadOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::WrapOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::UnwrapOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::OptionalCastOp>(op, ctx).is_some()
        || Operation::get_op::<crate::optional::ToNumberOp>(op, ctx).is_some()
        || Operation::get_op::<AwaitOp>(op, ctx).is_some()
    {
        return false;
    }
    if let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx) {
        return match intrinsic.intrinsic(ctx) {
            Some(
                crate::intrinsics::Intrinsic::BigIntNegate
                | crate::intrinsics::Intrinsic::BytesAt
                | crate::intrinsics::Intrinsic::ParseInt,
            ) => true,
            Some(
                crate::intrinsics::Intrinsic::MathFloor
                | crate::intrinsics::Intrinsic::MathCeil
                | crate::intrinsics::Intrinsic::MathTrunc
                | crate::intrinsics::Intrinsic::MathRound
                | crate::intrinsics::Intrinsic::MathAbs,
            ) => state.representation(op.deref(ctx).get_operand(0)).is_some(),
            Some(crate::intrinsics::Intrinsic::MathMin | crate::intrinsics::Intrinsic::MathMax) => {
                state.representation(op.deref(ctx).get_operand(0)).is_some()
                    && state.representation(op.deref(ctx).get_operand(1)).is_some()
            }
            _ => false,
        };
    }
    true
}

fn coalesce_key(
    ctx: &mut Context,
    analysis: &ProgramFacts,
    id: usize,
    key: &super::Key,
    body: &super::Body,
    state: &super::State,
) -> CoalesceKey {
    let mut values = Vec::new();
    let mut value_specs = Vec::new();
    let mut compares = Vec::new();
    let mut numeric_compares = Vec::new();
    let mut constrained_bins = Vec::new();
    let mut calls = Vec::new();
    for &constraint in &body.constraints {
        match constraint {
            Constraint::Op { op, .. } => {
                let num_results = op.deref(ctx).get_num_results();
                for idx in 0..num_results {
                    let res = op.deref(ctx).get_result(idx);
                    let can = can_specialize_value(ctx, state, res);
                    values.push(can.then(|| state.representation(res)).flatten());
                    value_specs.push(analysis.specialized_value_type(ctx, id, res));
                }
                if let Some(bin) = Operation::get_op::<BinaryOp>(op, ctx)
                    && matches!(
                        bin.kind(ctx),
                        crate::attrs::BinaryKind::Add
                            | crate::attrs::BinaryKind::Sub
                            | crate::attrs::BinaryKind::Mul
                    )
                {
                    constrained_bins.push(state.is_constrained(op.deref(ctx).get_result(0)));
                }
                if Operation::get_op::<CompareOp>(op, ctx).is_some() {
                    let inputs = [op.deref(ctx).get_operand(0), op.deref(ctx).get_operand(1)];
                    compares.push(comparison_domain(state, &inputs));
                    numeric_compares.push(
                        inputs
                            .iter()
                            .any(|&v| state.fact(v) == super::Fact::Numeric),
                    );
                }
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
            Constraint::Edge { to, .. } => {
                values.push(state.representation(to));
                value_specs.push(analysis.specialized_value_type(ctx, id, to));
            }
        }
    }
    let sig = TypedHandle::<FunctionType>::from_handle(body.function.get_type(ctx), ctx)
        .expect("function type");
    let res_types = sig.deref(ctx).res_types().clone();
    let arg_specs = body
        .parameters
        .iter()
        .map(|&p| analysis.specialized_value_type(ctx, id, p))
        .collect();
    let result_specs = res_types
        .iter()
        .enumerate()
        .map(|(pos, &ty)| analysis.specialized_result_type(ctx, id, pos, ty))
        .collect();
    CoalesceKey {
        function: key.function.clone(),
        arg_kinds: key
            .args
            .iter()
            .map(|a| a.map(|i| i.bits == 64 && !i.signed))
            .collect(),
        arg_specs,
        results: state.results.iter().map(|r| r.domain.integer()).collect(),
        result_specs,
        values,
        value_specs,
        compares,
        numeric_compares,
        constrained_bins,
        calls,
    }
}

fn rewrite(ctx: &mut Context, analysis: &ProgramFacts) -> usize {
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let mut reachable = HashSet::new();
    let mut pending: Vec<_> = analysis.roots.iter().map(|(_, id)| *id).collect();
    let root_ids: HashSet<usize> = pending.iter().copied().collect();
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(analysis.states[id].calls.values().copied());
        }
    }
    let mut changed = HashSet::new();
    let mut pending = Vec::new();
    for &id in &reachable {
        if !analysis.keys[id].specialize {
            continue;
        }
        let state = &analysis.states[id];
        let body = &analysis.bodies[&analysis.keys[id].function];
        let signature = body.function.get_type(ctx);
        let signature =
            TypedHandle::<FunctionType>::from_handle(signature, ctx).expect("function type");
        let arg_types = signature.deref(ctx).arg_types().clone();
        let res_types = signature.deref(ctx).res_types().clone();
        let argument_changes = analysis.keys[id]
            .args
            .iter()
            .zip(&arg_types)
            .any(|(domain, &ty)| domain.is_some_and(|domain| domain.ty(ctx) != ty))
            || body
                .parameters
                .iter()
                .any(|&p| analysis.specialized_value_type(ctx, id, p).is_some());
        let result_changes =
            state
                .results
                .iter()
                .enumerate()
                .zip(&res_types)
                .any(|((pos, result), &ty)| {
                    result
                        .domain
                        .integer()
                        .is_some_and(|domain| Integer::from_type(ctx, ty) != Some(domain))
                        || (!root_ids.contains(&id)
                            && !ty.deref(ctx).is::<PromiseType>()
                            && analysis.specialized_result_type(ctx, id, pos, ty).is_some())
                });
        let fact_keys: Vec<_> = state.facts.keys().copied().collect();
        let heap_keys: Vec<_> = state.heap_values.keys().copied().collect();
        let values_change = fact_keys.into_iter().any(|value| {
            (can_specialize_value(ctx, state, value)
                && state.representation(value).is_some_and(|domain| {
                    let ty = value.get_type(ctx);
                    let ty = TypedHandle::<PromiseType>::from_handle(ty, ctx)
                        .map_or(ty, |promise| promise.deref(ctx).value());
                    Integer::from_type(ctx, ty) != Some(domain)
                }))
                || (!value.get_type(ctx).deref(ctx).is::<PromiseType>()
                    && analysis.specialized_value_type(ctx, id, value).is_some())
        }) || heap_keys
            .into_iter()
            .any(|value| analysis.specialized_value_type(ctx, id, value).is_some())
            || body.constraints.iter().any(|&constraint| {
                matches!(
                    constraint,
                    Constraint::Op { op, .. } if super::domains::const_parsed_int(ctx, op).is_some()
                )
            });
        let comparisons_change = body.constraints.iter().any(|&constraint| {
            let Constraint::Op { op, .. } = constraint else {
                return false;
            };
            if Operation::get_op::<CompareOp>(op, ctx).is_none() {
                return false;
            };
            let lhs = op.deref(ctx).get_operand(0);
            let rhs = op.deref(ctx).get_operand(1);
            let lhs_ty = lhs.get_type(ctx);
            comparison_domain(state, &[lhs, rhs]).is_some_and(|domain| domain.ty(ctx) != lhs_ty)
                || state.fact(lhs) == super::Fact::Numeric
                || state.fact(rhs) == super::Fact::Numeric
        });
        if argument_changes || result_changes || values_change || comparisons_change {
            changed.insert(id);
            pending.push(id);
        }
    }
    while let Some(id) = pending.pop() {
        for &(caller, _) in &analysis.states[id].callers {
            if analysis.keys[caller].specialize
                && reachable.contains(&caller)
                && changed.insert(caller)
            {
                pending.push(caller);
            }
        }
    }
    let mut unique_impls: Vec<Implementation> = Vec::new();
    let mut impl_of: HashMap<usize, usize> = HashMap::new();
    let mut coalesced_map: HashMap<CoalesceKey, usize> = HashMap::new();
    for (id, key) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let body = &analysis.bodies[&key.function];
        let state = &analysis.states[id];
        let signature = TypedHandle::<FunctionType>::from_handle(body.function.get_type(ctx), ctx)
            .expect("function type");
        if changed.contains(&id) && !root_ids.contains(&id) {
            let ckey = coalesce_key(ctx, analysis, id, key, body, state);
            if let Some(&impl_idx) = coalesced_map.get(&ckey) {
                let imp = &mut unique_impls[impl_idx];
                let mut widened = false;
                for (pos, (&new_dom, cur_dom)) in
                    key.args.iter().zip(imp.arg_domains.iter_mut()).enumerate()
                {
                    if let (Some(new_int), Some(cur_int)) = (new_dom, *cur_dom) {
                        let common = cur_int.common(new_int);
                        if common != cur_int {
                            *cur_dom = Some(common);
                            imp.arguments[pos] = common.ty(ctx);
                            widened = true;
                        }
                    }
                }
                if widened {
                    let sig = FunctionType::get(ctx, imp.arguments.clone(), imp.results.clone());
                    imp.function
                        .set_attr_builtin_func_type(ctx, TypeAttr::new(sig.into()));
                    let entry = imp.function.get_entry_block(ctx);
                    for (index, &ty) in imp.arguments.iter().enumerate() {
                        let argument = entry.deref(ctx).get_argument(index);
                        argument.set_type(ctx, ty);
                    }
                }
                impl_of.insert(id, impl_idx);
                continue;
            }
        }
        let mut implementation = Implementation {
            function: body.function,
            mapping: IrMapping::new(),
            arg_domains: key.args.clone(),
            arguments: signature.deref(ctx).arg_types(),
            results: signature.deref(ctx).res_types(),
            primary_context: id,
        };
        if changed.contains(&id) {
            let cloned = clone_operation(
                body.function.get_operation(),
                ctx,
                &mut rewriter,
                &mut implementation.mapping,
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
            for (pos, (ty, domain)) in implementation
                .arguments
                .iter_mut()
                .zip(&key.args)
                .enumerate()
            {
                if let Some(domain) = domain {
                    *ty = domain.ty(ctx);
                } else if let Some(&param) = body.parameters.get(pos)
                    && let Some(spec_ty) = analysis.specialized_value_type(ctx, id, param)
                {
                    *ty = spec_ty;
                }
            }
            for (pos, (ty, facts)) in implementation
                .results
                .iter_mut()
                .zip(&analysis.states[id].results)
                .enumerate()
            {
                if let Some(domain) = facts.domain.integer() {
                    *ty = domain.ty(ctx);
                } else if !root_ids.contains(&id)
                    && !ty.deref(ctx).is::<PromiseType>()
                    && let Some(spec_ty) = analysis.specialized_result_type(ctx, id, pos, *ty)
                {
                    *ty = spec_ty;
                }
            }
            let signature = FunctionType::get(
                ctx,
                implementation.arguments.clone(),
                implementation.results.clone(),
            );
            function.set_attr_builtin_func_type(ctx, TypeAttr::new(signature.into()));
            let entry = function.get_entry_block(ctx);
            for (index, &ty) in implementation.arguments.iter().enumerate() {
                let argument = entry.deref(ctx).get_argument(index);
                argument.set_type(ctx, ty);
            }
            implementation.function = function;
        }
        let impl_idx = unique_impls.len();
        if changed.contains(&id) && !root_ids.contains(&id) {
            let ckey = coalesce_key(ctx, analysis, id, key, body, state);
            coalesced_map.insert(ckey, impl_idx);
        }
        unique_impls.push(implementation);
        impl_of.insert(id, impl_idx);
    }
    for impl_idx in 0..unique_impls.len() {
        let id = unique_impls[impl_idx].primary_context;
        if !changed.contains(&id) {
            continue;
        }
        let implementation = &unique_impls[impl_idx];
        let mapping = &implementation.mapping;
        let state = &analysis.states[id];
        let body = &analysis.bodies[&analysis.keys[id].function];
        let all_values: Vec<_> = state
            .facts
            .keys()
            .copied()
            .chain(
                state
                    .heap_values
                    .keys()
                    .copied()
                    .filter(|v| !state.facts.contains_key(v)),
            )
            .collect();
        for value in all_values {
            if body.parameters.contains(&value) {
                continue;
            }
            if can_specialize_value(ctx, state, value)
                && let Some(domain) = state.representation(value)
            {
                let cloned = mapping.lookup_value(value).expect("cloned value");
                let scalar = domain.ty(ctx);
                let ty = if value.get_type(ctx).deref(ctx).is::<PromiseType>() {
                    PromiseType::get(ctx, scalar).into()
                } else {
                    scalar
                };
                cloned.set_type(ctx, ty);
            } else if !value.get_type(ctx).deref(ctx).is::<PromiseType>()
                && let Some(spec_ty) = analysis.specialized_value_type(ctx, id, value)
            {
                let cloned = mapping.lookup_value(value).expect("cloned value");
                cloned.set_type(ctx, spec_ty);
            }
        }
        let mut expected = Vec::new();
        for &constraint in &body.constraints {
            let Constraint::Op { op: original, .. } = constraint else {
                continue;
            };
            let op = mapping.lookup_op(original).expect("cloned operation");
            let result = original.deref(ctx).results().next();
            rewriter.set_insertion_point(OpInsertionPoint::BeforeOperation(op));
            if let Some(value) = result
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
            expected.clear();
            expected.extend(
                original
                    .deref(ctx)
                    .operands()
                    .map(|value| value.get_type(ctx)),
            );
            if let Some(intrinsic) =
                Operation::get_op::<crate::intrinsics::IntrinsicOp>(original, ctx)
                    .and_then(|op| op.intrinsic(ctx))
            {
                let first_arg = op.deref(ctx).operands().next();
                let orig_arg = original.deref(ctx).operands().next();
                if let Some((value, orig_arg)) = first_arg.zip(orig_arg)
                    && (Integer::from_type(ctx, value.get_type(ctx)).is_some()
                        || state.representation(orig_arg).is_some())
                    && matches!(
                        intrinsic,
                        crate::intrinsics::Intrinsic::BigIntToNumber
                            | crate::intrinsics::Intrinsic::MathFloor
                            | crate::intrinsics::Intrinsic::MathCeil
                            | crate::intrinsics::Intrinsic::MathTrunc
                            | crate::intrinsics::Intrinsic::MathRound
                    )
                    && (intrinsic == crate::intrinsics::Intrinsic::BigIntToNumber
                        || Integer::from_type(ctx, op.deref(ctx).get_result(0).get_type(ctx))
                            .is_some())
                {
                    let ty = op.deref(ctx).get_result(0).get_type(ctx);
                    let cast = NumericCastOp::new(ctx, value, ty).get_operation();
                    rewriter.insert_operation(ctx, cast);
                    rewriter.replace_operation(ctx, op, cast);
                    continue;
                }
                if intrinsic == crate::intrinsics::Intrinsic::BigIntNegate
                    && let Some(value) = first_arg
                    && let Some(domain) = state.representation(result.expect("intrinsic result"))
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
                if intrinsic == crate::intrinsics::Intrinsic::ParseInt {
                    if let Some(parsed) = super::domains::const_parsed_int(ctx, original) {
                        let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                        let attr: Box<dyn pliron::builtin::attr_interfaces::TypedAttrInterface> =
                            if res_ty.deref(ctx).is::<crate::types::NumericType>() {
                                match parsed {
                                    super::domains::ConstParsedInt::Integer {
                                        bits,
                                        domain,
                                        ..
                                    } => {
                                        if domain.signed {
                                            Box::new(crate::attrs::NumericConstant::Signed(
                                                bits as i64,
                                            ))
                                        } else {
                                            Box::new(crate::attrs::NumericConstant::Unsigned(bits))
                                        }
                                    }
                                    super::domains::ConstParsedInt::Float(bits) => {
                                        Box::new(crate::attrs::NumericConstant::Float(bits))
                                    }
                                }
                            } else {
                                let f = match parsed {
                                    super::domains::ConstParsedInt::Integer {
                                        bits,
                                        domain,
                                        ..
                                    } => {
                                        if domain.signed {
                                            (bits as i64) as f64
                                        } else {
                                            bits as f64
                                        }
                                    }
                                    super::domains::ConstParsedInt::Float(bits) => {
                                        f64::from_bits(bits)
                                    }
                                };
                                Box::new(pliron::builtin::attributes::FPDoubleAttr::from(f))
                            };
                        let constant = ConstantOp::new(ctx, attr).get_operation();
                        rewriter.insert_operation(ctx, constant);
                        rewriter.replace_operation(ctx, op, constant);
                        continue;
                    }
                    if op
                        .deref(ctx)
                        .get_result(0)
                        .get_type(ctx)
                        .deref(ctx)
                        .is::<crate::types::NumericType>()
                    {
                        expected[1] = crate::types::NumericType::get(ctx).into();
                    }
                }
                if matches!(
                    intrinsic,
                    crate::intrinsics::Intrinsic::NumberToString
                        | crate::intrinsics::Intrinsic::NumberIsNaN
                        | crate::intrinsics::Intrinsic::NumberIsFinite
                        | crate::intrinsics::Intrinsic::NumberIsInteger
                        | crate::intrinsics::Intrinsic::NumberIsSafeInteger
                ) {
                    let arg_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                    if arg_ty.deref(ctx).is::<crate::types::NumericType>()
                        || Integer::from_type(ctx, arg_ty).is_some()
                    {
                        expected[0] = arg_ty;
                    }
                }
                if matches!(
                    intrinsic,
                    crate::intrinsics::Intrinsic::MathFloor
                        | crate::intrinsics::Intrinsic::MathCeil
                        | crate::intrinsics::Intrinsic::MathTrunc
                        | crate::intrinsics::Intrinsic::MathRound
                        | crate::intrinsics::Intrinsic::MathAbs
                        | crate::intrinsics::Intrinsic::MathMin
                        | crate::intrinsics::Intrinsic::MathMax
                ) {
                    let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                    if Integer::from_type(ctx, res_ty).is_some()
                        || res_ty.deref(ctx).is::<crate::types::NumericType>()
                    {
                        expected.fill(res_ty);
                    }
                }
            }
            if let Some(&child) = state.calls.get(&original) {
                let callee = &unique_impls[impl_of[&child]];
                let name = IdentifierAttr::new(callee.function.get_symbol_name(ctx));
                let asynchronous = if let Some(call) = Operation::get_op::<AsyncCallOp>(op, ctx) {
                    call.set_attr_perry_async_callee(ctx, name);
                    true
                } else {
                    Operation::get_op::<CallOp>(op, ctx)
                        .expect("call")
                        .set_attr_perry_callee(ctx, name);
                    false
                };
                expected.clone_from(&callee.arguments);
                let results = op.deref(ctx).get_num_results();
                for (index, ty) in (0..results).zip(&callee.results) {
                    let value = op.deref(ctx).get_result(index);
                    let ty = if asynchronous {
                        PromiseType::get(ctx, *ty).into()
                    } else {
                        *ty
                    };
                    value.set_type(ctx, ty);
                }
            } else if Operation::get_op::<AwaitOp>(original, ctx).is_some() {
                let promise = op.deref(ctx).get_operand(0).get_type(ctx);
                let payload = TypedHandle::<PromiseType>::from_handle(promise, ctx)
                    .expect("await promise")
                    .deref(ctx)
                    .value();
                let result = op.deref(ctx).get_result(0);
                result.set_type(ctx, payload);
                expected[0] = promise;
            } else if let Some(index) = promise_payload(ctx, original) {
                expected[index] = TypedHandle::<PromiseType>::from_handle(
                    op.deref(ctx).get_result(0).get_type(ctx),
                    ctx,
                )
                .expect("promise")
                .deref(ctx)
                .value();
            } else if Operation::get_op::<crate::heap::AllocOp>(original, ctx).is_some() {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some(fields) = res_ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::RecordType>()
                    .and_then(|r| r.fields().ok())
                {
                    for (i, (_, fty)) in fields.iter().enumerate() {
                        if i < expected.len() {
                            expected[i] = *fty;
                        }
                    }
                }
            } else if Operation::get_op::<crate::heap::FieldGetOp>(original, ctx).is_some()
                || Operation::get_op::<crate::heap::FieldPresentOp>(original, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayGetOp>(original, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayAtOp>(original, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayLenOp>(original, ctx).is_some()
                || Operation::get_op::<crate::collections::ArrayTruncateOp>(original, ctx).is_some()
                || Operation::get_op::<crate::optional::IsUndefinedOp>(original, ctx).is_some()
                || Operation::get_op::<crate::optional::IsNullOp>(original, ctx).is_some()
            {
                expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
            } else if Operation::get_op::<crate::optional::UnwrapOp>(original, ctx).is_some()
                || Operation::get_op::<crate::optional::ToNumberOp>(original, ctx).is_some()
            {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                let orig_arg_ty = original.deref(ctx).get_operand(0).get_type(ctx);
                if let Some(absence) = crate::optional::absence(ctx, orig_arg_ty) {
                    expected[0] = crate::types::OptionalType::get_with(ctx, res_ty, absence).into();
                } else {
                    expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
                }
            } else if Operation::get_op::<crate::optional::OptionalCastOp>(original, ctx).is_some()
            {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                let orig_arg_ty = original.deref(ctx).get_operand(0).get_type(ctx);
                if let (Some(inner), Some(absence)) = (
                    crate::optional::optional_value(ctx, res_ty),
                    crate::optional::absence(ctx, orig_arg_ty),
                ) {
                    expected[0] = crate::types::OptionalType::get_with(ctx, inner, absence).into();
                } else {
                    expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
                }
            } else if let Some(fs) = Operation::get_op::<crate::heap::FieldSetOp>(original, ctx) {
                let base_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                expected[0] = base_ty;
                let field_name = fs.field(ctx);
                if let Some((_, fty)) = base_ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::RecordType>()
                    .and_then(|r| r.fields().ok())
                    .and_then(|f| f.iter().find(|(n, _)| *n == field_name))
                {
                    expected[1] = *fty;
                }
            } else if Operation::get_op::<crate::collections::ArrayNewOp>(original, ctx).is_some() {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some(elem_ty) = res_ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::ArrayType>()
                    .map(|a| a.element())
                {
                    expected.fill(elem_ty);
                }
            } else if Operation::get_op::<crate::collections::ArrayPushOp>(original, ctx).is_some()
            {
                let base_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                expected[0] = base_ty;
                if let Some(elem_ty) = base_ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::ArrayType>()
                    .map(|a| a.element())
                {
                    expected[1] = elem_ty;
                }
            } else if Operation::get_op::<crate::collections::ArraySetOp>(original, ctx).is_some() {
                let base_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                expected[0] = base_ty;
                if let Some(elem_ty) = base_ty
                    .deref(ctx)
                    .downcast_ref::<crate::types::ArrayType>()
                    .map(|a| a.element())
                {
                    expected[2] = elem_ty;
                }
            } else if Operation::get_op::<crate::collections::ArrayExtendOp>(original, ctx)
                .is_some()
            {
                expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
                expected[1] = op.deref(ctx).get_operand(1).get_type(ctx);
            } else if Operation::get_op::<crate::optional::WrapOp>(original, ctx).is_some() {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                if let Some(inner_ty) = crate::optional::optional_value(ctx, res_ty) {
                    expected[0] = inner_ty;
                }
            } else if Operation::get_op::<BinaryOp>(original, ctx).is_some() {
                let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
                expected.fill(res_ty);
                if res_ty.deref(ctx).is::<crate::types::NumericType>()
                    && let Some(cloned_bin) = Operation::get_op::<BinaryOp>(op, ctx)
                    && matches!(
                        cloned_bin.kind(ctx),
                        crate::attrs::BinaryKind::Add
                            | crate::attrs::BinaryKind::Sub
                            | crate::attrs::BinaryKind::Mul
                    )
                {
                    let orig_res = original.deref(ctx).get_result(0);
                    cloned_bin.set_unconstrained(ctx, !state.is_constrained(orig_res));
                }
            } else if Operation::get_op::<CompareOp>(original, ctx).is_some() {
                let inputs = [
                    original.deref(ctx).get_operand(0),
                    original.deref(ctx).get_operand(1),
                ];
                if let Some(domain) = comparison_domain(state, &inputs) {
                    expected.fill(domain.ty(ctx));
                } else {
                    let l_ty = op.deref(ctx).get_operand(0).get_type(ctx);
                    let r_ty = op.deref(ctx).get_operand(1).get_type(ctx);
                    let is_num = |t: TypeHandle| {
                        let inner = crate::optional::optional_value(ctx, t).unwrap_or(t);
                        inner.deref(ctx).is::<crate::types::NumericType>()
                    };
                    if is_num(l_ty) || is_num(r_ty) {
                        if let Some(absence) = crate::optional::absence(ctx, l_ty)
                            .or_else(|| crate::optional::absence(ctx, r_ty))
                        {
                            let target = crate::types::OptionalType::get_with(
                                ctx,
                                crate::types::NumericType::get(ctx).into(),
                                absence,
                            )
                            .into();
                            expected.fill(target);
                        } else {
                            expected.fill(crate::types::NumericType::get(ctx).into());
                        }
                    }
                }
            } else if Operation::get_op::<ReturnOp>(original, ctx).is_some() {
                expected.clone_from(&implementation.results);
            } else if Operation::get_op::<NumericCastOp>(original, ctx).is_some()
                || Operation::get_op::<crate::intrinsics::JsonStringifyOp>(original, ctx).is_some()
            {
                expected[0] = op.deref(ctx).get_operand(0).get_type(ctx);
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
            for (index, &ty) in expected.iter().enumerate() {
                let value = op.deref(ctx).get_operand(index);
                if value.get_type(ctx) != ty {
                    if let Some(def_op) = value.defining_op() {
                        if Operation::get_op::<crate::optional::UndefinedOp>(def_op, ctx).is_some()
                        {
                            let undef = crate::optional::UndefinedOp::with_type(ctx, ty);
                            rewriter.insert_operation(ctx, undef.get_operation());
                            Operation::replace_operand(op, ctx, index, undef.get_result(ctx));
                            continue;
                        }
                        if Operation::get_op::<crate::optional::OmittedOp>(def_op, ctx).is_some() {
                            let omit = crate::optional::OmittedOp::with_type(ctx, ty);
                            rewriter.insert_operation(ctx, omit.get_operation());
                            Operation::replace_operand(op, ctx, index, omit.get_result(ctx));
                            continue;
                        }
                        if Operation::get_op::<crate::optional::NullOp>(def_op, ctx).is_some() {
                            let nul = crate::optional::NullOp::new(ctx, ty);
                            rewriter.insert_operation(ctx, nul.get_operation());
                            Operation::replace_operand(op, ctx, index, nul.get_result(ctx));
                            continue;
                        }
                    }
                    let mut value = value;
                    if ty.deref(ctx).is::<crate::types::NumericType>()
                        && let Some(&child) = state.calls.get(&original)
                        && let Some(val_int) = Integer::from_type(ctx, value.get_type(ctx))
                        && analysis.bodies[&analysis.keys[child].function]
                            .parameters
                            .get(index)
                            .is_some_and(|&p| analysis.states[child].is_constrained(p))
                        && let Some(orig_res) = result
                    {
                        let widened_int = if !state.is_constrained(orig_res) {
                            val_int.unconstrained()
                        } else if let Some(req_int) = state
                            .required
                            .get(&orig_res)
                            .copied()
                            .or_else(|| state.representation(orig_res))
                        {
                            let common = val_int.common(req_int);
                            if val_int.signed && !common.signed {
                                val_int.unconstrained()
                            } else {
                                common
                            }
                        } else {
                            val_int
                        };
                        if widened_int != val_int {
                            let mid_ty = widened_int.ty(ctx);
                            let mid_cast = NumericCastOp::new(ctx, value, mid_ty);
                            rewriter.insert_operation(ctx, mid_cast.get_operation());
                            value = mid_cast.get_result(ctx);
                        }
                    }
                    let cast = NumericCastOp::new(ctx, value, ty);
                    rewriter.insert_operation(ctx, cast.get_operation());
                    Operation::replace_operand(op, ctx, index, cast.get_result(ctx));
                }
            }
        }
    }
    for &(original, id) in &analysis.roots {
        if !changed.contains(&id) {
            continue;
        }
        let implementation = &unique_impls[impl_of[&id]];
        if !original
            .get_operation()
            .deref(ctx)
            .attributes
            .0
            .contains_key(&WIT_SIGNATURE)
            && analysis.states[id].callers.is_empty()
            && analysis
                .generic
                .get(&original.get_symbol_name(ctx))
                .is_none_or(|gid| !reachable.contains(gid))
        {
            let orig_name = original.get_symbol_name(ctx);
            Operation::erase(original.get_operation(), ctx);
            implementation.function.set_symbol_name(ctx, orig_name);
            continue;
        }
        crate::calls::set_specialized_entry(ctx, &implementation.function);
        let name = implementation.function.get_symbol_name(ctx);
        original
            .get_operation()
            .deref_mut(ctx)
            .attributes
            .set(SPECIALIZATION.clone(), IdentifierAttr::new(name));
    }
    changed.len()
}
