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
    identifier::{Identifier, Legaliser},
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

mod operands;

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
        let previous = crate::pipeline::optimization_statistics(root, ctx);
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

/// One emitted function body, shared by every context whose `CoalesceKey` matches.
struct Implementation {
    function: FuncOp,
    mapping: IrMapping,
    arg_domains: Vec<Option<Integer>>,
    arguments: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
    primary_context: usize,
}

/// Unique implementations and the implementation chosen for each reachable context.
#[derive(Default)]
struct Implementations {
    unique: Vec<Implementation>,
    of: HashMap<usize, usize>,
}

impl Implementations {
    fn of(&self, id: usize) -> &Implementation {
        &self.unique[self.of[&id]]
    }
}

/// Everything that decides a context's rewritten body; equal keys share one implementation.
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

fn coalesce_key(ctx: &Context, analysis: &ProgramFacts, id: usize) -> CoalesceKey {
    let key = &analysis.keys[id];
    let body = &analysis.bodies[&key.function];
    let state = &analysis.states[id];
    let mut values = Vec::new();
    let mut value_specs = Vec::new();
    let mut compares = Vec::new();
    let mut numeric_compares = Vec::new();
    let mut constrained_bins = Vec::new();
    let mut calls = Vec::new();
    for &constraint in &body.constraints {
        match constraint {
            Constraint::Op { op, .. } => {
                for res in op.deref(ctx).results() {
                    let can = can_specialize_value(ctx, state, res);
                    values.push(can.then(|| state.representation(res)).flatten());
                    value_specs.push(analysis.specialized_value_type(ctx, id, res));
                }
                if let Some(bin) = Operation::get_op::<BinaryOp>(op, ctx)
                    && matches!(
                        bin.kind(ctx),
                        BinaryKind::Add | BinaryKind::Sub | BinaryKind::Mul
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
    let arg_specs = body
        .parameters
        .iter()
        .map(|&p| analysis.specialized_value_type(ctx, id, p))
        .collect();
    // Owned first: specializing may instantiate types while a signature borrow is live.
    let res_types = body.signature.deref(ctx).res_types();
    let result_specs = res_types
        .into_iter()
        .enumerate()
        .map(|(pos, ty)| analysis.specialized_result_type(ctx, id, pos, ty))
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
    let root_ids: HashSet<usize> = analysis.roots.iter().map(|&(_, id)| id).collect();
    let reachable = reachable_contexts(analysis, &root_ids);
    let changed = changed_contexts(ctx, analysis, &reachable, &root_ids);
    let mut implementations = Implementations::default();
    let mut coalesced_map: HashMap<CoalesceKey, usize> = HashMap::new();
    for (id, key) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let body = &analysis.bodies[&key.function];
        let ckey = (changed.contains(&id) && !root_ids.contains(&id))
            .then(|| coalesce_key(ctx, analysis, id));
        if let Some(&impl_idx) = ckey.as_ref().and_then(|ckey| coalesced_map.get(ckey)) {
            widen_arguments(ctx, &mut implementations.unique[impl_idx], &key.args);
            implementations.of.insert(id, impl_idx);
            continue;
        }
        let mut implementation = Implementation {
            function: body.function,
            mapping: IrMapping::new(),
            arg_domains: key.args.clone(),
            arguments: body.signature.deref(ctx).arg_types(),
            results: body.signature.deref(ctx).res_types(),
            primary_context: id,
        };
        if changed.contains(&id) {
            clone_implementation(
                ctx,
                analysis,
                &mut rewriter,
                &mut implementation,
                root_ids.contains(&id),
            );
        }
        let impl_idx = implementations.unique.len();
        if let Some(ckey) = ckey {
            coalesced_map.insert(ckey, impl_idx);
        }
        implementations.unique.push(implementation);
        implementations.of.insert(id, impl_idx);
    }
    for implementation in &implementations.unique {
        if changed.contains(&implementation.primary_context) {
            operands::rewrite_implementation(
                ctx,
                analysis,
                &mut rewriter,
                &implementations,
                implementation,
            );
        }
    }
    for &(original, id) in &analysis.roots {
        if !changed.contains(&id) {
            continue;
        }
        let implementation = implementations.of(id);
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

/// Contexts called, transitively, from the specialized roots.
fn reachable_contexts(analysis: &ProgramFacts, root_ids: &HashSet<usize>) -> HashSet<usize> {
    let mut reachable = HashSet::new();
    let mut pending: Vec<usize> = root_ids.iter().copied().collect();
    while let Some(id) = pending.pop() {
        if reachable.insert(id) {
            pending.extend(analysis.states[id].calls.values().copied());
        }
    }
    reachable
}

/// Reachable specialized contexts whose types change, plus their specialized callers, which
/// must call the rewritten clone.
fn changed_contexts(
    ctx: &Context,
    analysis: &ProgramFacts,
    reachable: &HashSet<usize>,
    root_ids: &HashSet<usize>,
) -> HashSet<usize> {
    let mut changed = HashSet::new();
    let mut pending = Vec::new();
    for &id in reachable {
        if analysis.keys[id].specialize && types_change(ctx, analysis, id, root_ids) {
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
    changed
}

/// Whether specializing context `id` changes any argument, result, value or comparison type.
fn types_change(
    ctx: &Context,
    analysis: &ProgramFacts,
    id: usize,
    root_ids: &HashSet<usize>,
) -> bool {
    let state = &analysis.states[id];
    let body = &analysis.bodies[&analysis.keys[id].function];
    // Owned first: specializing may instantiate types while a signature borrow is live.
    let arg_types = body.signature.deref(ctx).arg_types();
    let res_types = body.signature.deref(ctx).res_types();
    let argument_changes = analysis.keys[id]
        .args
        .iter()
        .zip(arg_types)
        .any(|(domain, ty)| domain.is_some_and(|domain| domain.ty(ctx) != ty))
        || body
            .parameters
            .iter()
            .any(|&p| analysis.specialized_value_type(ctx, id, p).is_some());
    let result_changes =
        state
            .results
            .iter()
            .enumerate()
            .zip(res_types)
            .any(|((pos, result), ty)| {
                result
                    .domain
                    .integer()
                    .is_some_and(|domain| Integer::from_type(ctx, ty) != Some(domain))
                    || (!root_ids.contains(&id)
                        && !ty.deref(ctx).is::<PromiseType>()
                        && analysis.specialized_result_type(ctx, id, pos, ty).is_some())
            });
    let values_change = state.facts.keys().any(|&value| {
        (can_specialize_value(ctx, state, value)
            && state.representation(value).is_some_and(|domain| {
                let ty = value.get_type(ctx);
                let ty = TypedHandle::<PromiseType>::from_handle(ty, ctx)
                    .map_or(ty, |promise| promise.deref(ctx).value());
                Integer::from_type(ctx, ty) != Some(domain)
            }))
            || (!value.get_type(ctx).deref(ctx).is::<PromiseType>()
                && analysis.specialized_value_type(ctx, id, value).is_some())
    }) || state
        .heap_values
        .keys()
        .any(|&value| analysis.specialized_value_type(ctx, id, value).is_some())
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
    argument_changes || result_changes || values_change || comparisons_change
}

/// Widens a coalesced implementation's integer arguments to also accept `args`.
fn widen_arguments(ctx: &mut Context, imp: &mut Implementation, args: &[Option<Integer>]) {
    let mut widened = false;
    for (pos, (&new_dom, cur_dom)) in args.iter().zip(imp.arg_domains.iter_mut()).enumerate() {
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
        set_signature(ctx, imp);
    }
}

/// Clones the context's function next to the original and gives it the specialized signature.
fn clone_implementation(
    ctx: &mut Context,
    analysis: &ProgramFacts,
    rewriter: &mut IRRewriter<DummyListener>,
    implementation: &mut Implementation,
    is_root: bool,
) {
    let id = implementation.primary_context;
    let key = &analysis.keys[id];
    let body = &analysis.bodies[&key.function];
    let cloned = clone_operation(
        body.function.get_operation(),
        ctx,
        rewriter,
        &mut implementation.mapping,
    );
    let function = Operation::get_op::<FuncOp>(cloned, ctx).expect("cloned function");
    // Legalising a prefix of identifier characters plus an existing identifier is
    // the identity, so names stay as before without a fallible conversion.
    let mut name = Legaliser::default().legalise(&format!("__perry_integer_{id}_{}", key.function));
    while analysis.bodies.contains_key(&name) {
        name = Legaliser::default().legalise(&format!("_{name}"));
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
        } else if !is_root
            && !ty.deref(ctx).is::<PromiseType>()
            && let Some(spec_ty) = analysis.specialized_result_type(ctx, id, pos, *ty)
        {
            *ty = spec_ty;
        }
    }
    implementation.function = function;
    set_signature(ctx, implementation);
}

/// Writes the implementation's argument and result types onto its function and entry block.
fn set_signature(ctx: &mut Context, imp: &Implementation) {
    let signature = FunctionType::get(ctx, imp.arguments.clone(), imp.results.clone());
    imp.function
        .set_attr_builtin_func_type(ctx, TypeAttr::new(signature.into()));
    let entry = imp.function.get_entry_block(ctx);
    for (index, &ty) in imp.arguments.iter().enumerate() {
        let argument = entry.deref(ctx).get_argument(index);
        argument.set_type(ctx, ty);
    }
}
