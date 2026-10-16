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
            let count = rewrite(ctx, &analysis)?;
            verify_lossless_module(root, ctx, &analysis)?;
            count
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

pub(in crate::facts) fn can_specialize_value(
    ctx: &Context,
    state: &super::State,
    value: pliron::value::Value,
) -> bool {
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
    let results = state
        .results
        .iter()
        .zip(&res_types)
        .map(|(r, &ty)| {
            (!ty.deref(ctx).is::<crate::types::OptionalType>()
                && !ty.deref(ctx).is::<PromiseType>())
            .then(|| r.domain.integer())
            .flatten()
        })
        .collect();
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
        results,
        result_specs,
        values,
        value_specs,
        compares,
        numeric_compares,
        constrained_bins,
        calls,
    }
}

fn rewrite(ctx: &mut Context, analysis: &ProgramFacts) -> Result<usize> {
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
    for &id in &reachable {
        if analysis.keys[id].specialize {
            let implementation = implementations.of(id);
            verify_lossless_context(
                ctx,
                analysis,
                id,
                implementation.function,
                Some(&implementation.mapping),
                root_ids.contains(&id),
            )?;
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
    Ok(changed.len())
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
                (!ty.deref(ctx).is::<crate::types::OptionalType>()
                    && !ty.deref(ctx).is::<PromiseType>()
                    && result
                        .domain
                        .integer()
                        .is_some_and(|domain| Integer::from_type(ctx, ty) != Some(domain)))
                    || ((!root_ids.contains(&id) || super::heap::aggregate(ctx, ty))
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
        let needs_numeric = |fact: super::Fact| {
            fact == super::Fact::Numeric || fact.integer().is_some_and(|int| int.bits == 64)
        };
        comparison_domain(state, &[lhs, rhs]).is_some_and(|domain| domain.ty(ctx) != lhs_ty)
            || needs_numeric(state.fact(lhs))
            || needs_numeric(state.fact(rhs))
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
        if !ty.deref(ctx).is::<crate::types::OptionalType>()
            && !ty.deref(ctx).is::<PromiseType>()
            && let Some(domain) = facts.domain.integer()
        {
            *ty = domain.ty(ctx);
        } else if (!is_root || super::heap::aggregate(ctx, *ty))
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

#[derive(Debug, thiserror::Error)]
#[error("lossless representation violated: {0}")]
pub(crate) struct LosslessRepresentationErr(pub String);

fn is_fp64_carrier(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
    inner.deref(ctx).is::<pliron::builtin::types::FP64Type>()
}

/// Verifies that no specialized function in `root` backs a `Fact::Numeric` or 64-bit integer
/// value or aggregate slot with a lossy `builtin.fp64` carrier.
pub fn verify_lossless_module(
    root: Ptr<Operation>,
    ctx: &Context,
    analysis: &ProgramFacts,
) -> Result<()> {
    let funcs: HashMap<Identifier, FuncOp> = crate::aggregates::functions(root, ctx)
        .into_iter()
        .map(|f| (f.get_symbol_name(ctx), f))
        .collect();
    let root_ids: HashSet<usize> = analysis.roots.iter().map(|&(_, id)| id).collect();
    for &(_, id) in &analysis.roots {
        let orig_name = &analysis.keys[id].function;
        let target_name = funcs
            .get(orig_name)
            .map(|f| super::specialized_implementation(ctx, f))
            .unwrap_or_else(|| orig_name.clone());
        if let Some(&func) = funcs.get(&target_name) {
            verify_lossless_context(ctx, analysis, id, func, None, root_ids.contains(&id))?;
        }
    }
    Ok(())
}

/// Verifies that no active function in `root` contains an operation whose lossless
/// (`perry.numeric` or 64-bit integer) result or field/element projection was degraded
/// to a `builtin.fp64` carrier after all MIR passes complete.
pub fn verify_lossless_operations(root: Ptr<Operation>, ctx: &Context) -> Result<()> {
    let functions = crate::aggregates::functions(root, ctx);
    let by_name: HashMap<Identifier, FuncOp> = functions
        .iter()
        .map(|&f| (f.get_symbol_name(ctx), f))
        .collect();
    let mut visited = HashSet::new();
    let mut pending = Vec::new();
    for &func in &functions {
        let name = func.get_symbol_name(ctx);
        let is_entry = super::wit_signature(ctx, &func).is_some()
            || crate::calls::is_exported(ctx, &func)
            || name.as_ref() == crate::globals::INITIALIZER
            || name.as_ref() == "perry_main";
        if is_entry {
            let target = super::specialized_implementation(ctx, &func);
            if visited.insert(target.clone()) {
                pending.push(target);
            }
        }
    }
    while let Some(name) = pending.pop() {
        let Some(&func) = by_name.get(&name) else {
            continue;
        };
        verify_lossless_function_ops(ctx, func)?;
        for op in crate::aggregates::operations(ctx, &func) {
            let callee = Operation::get_op::<CallOp>(op, ctx)
                .map(|c| c.callee(ctx))
                .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|c| c.callee(ctx)));
            if let Some(callee) = callee
                && visited.insert(callee.clone())
            {
                pending.push(callee);
            }
        }
    }
    Ok(())
}

fn is_lossless_type(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
    inner.deref(ctx).is::<crate::types::NumericType>()
        || Integer::from_type(ctx, inner).is_some_and(|d| d.bits == 64)
}

fn verify_lossless_function_ops(ctx: &Context, function: FuncOp) -> Result<()> {
    use pliron::verify_err;
    let loc = function.loc(ctx);
    for op in crate::aggregates::operations(ctx, &function) {
        if let Some(intrinsic) = Operation::get_op::<crate::intrinsics::IntrinsicOp>(op, ctx)
            && intrinsic.intrinsic(ctx) == Some(crate::intrinsics::Intrinsic::ParseInt)
            && is_fp64_carrier(ctx, op.deref(ctx).get_result(0).get_type(ctx))
        {
            return verify_err!(
                loc,
                LosslessRepresentationErr("ParseInt result has f64 carrier".into())
            );
        }
        if let Some(fg) = Operation::get_op::<crate::heap::FieldGetOp>(op, ctx) {
            let base_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let field_name = fg.field(ctx);
            let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
            if let Some(field_ty) = base_ty
                .deref(ctx)
                .downcast_ref::<crate::types::RecordType>()
                .and_then(|r| r.fields().ok().map(|f| f.to_vec()))
                .and_then(|fields| fields.into_iter().find(|(n, _)| *n == field_name))
                .map(|(_, t)| t)
                && is_lossless_type(ctx, field_ty)
                && is_fp64_carrier(ctx, res_ty)
            {
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(format!(
                        "field_get of lossless field `{field_name}` degraded to f64 carrier"
                    ))
                );
            }
        }
        if (Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some()
            || Operation::get_op::<crate::collections::ArrayAtOp>(op, ctx).is_some())
            && let Some(elem_ty) = op
                .deref(ctx)
                .get_operand(0)
                .get_type(ctx)
                .deref(ctx)
                .downcast_ref::<crate::types::ArrayType>()
                .map(|a| a.element())
            && is_lossless_type(ctx, elem_ty)
            && is_fp64_carrier(ctx, op.deref(ctx).get_result(0).get_type(ctx))
        {
            return verify_err!(
                loc,
                LosslessRepresentationErr(
                    "array element load of lossless element degraded to f64 carrier".into()
                )
            );
        }
        if Operation::get_op::<crate::optional::UnwrapOp>(op, ctx).is_some() {
            let opt_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
            if is_lossless_type(ctx, opt_ty) && is_fp64_carrier(ctx, res_ty) {
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(
                        "optional unwrap of lossless value degraded to f64 carrier".into()
                    )
                );
            }
        }
        if Operation::get_op::<crate::optional::WrapOp>(op, ctx).is_some() {
            let in_ty = op.deref(ctx).get_operand(0).get_type(ctx);
            let res_ty = op.deref(ctx).get_result(0).get_type(ctx);
            if is_lossless_type(ctx, in_ty) && is_fp64_carrier(ctx, res_ty) {
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(
                        "optional wrap of lossless value degraded to f64 carrier".into()
                    )
                );
            }
        }
    }
    Ok(())
}

fn verify_lossless_context(
    ctx: &Context,
    analysis: &ProgramFacts,
    id: usize,
    function: FuncOp,
    mapping: Option<&IrMapping>,
    is_root: bool,
) -> Result<()> {
    use pliron::{
        builtin::op_interfaces::OneRegionInterface, linked_list::ContainsLinkedList, verify_err,
    };
    let loc = function.loc(ctx);
    let key = &analysis.keys[id];
    let state = &analysis.states[id];
    let body = &analysis.bodies[&key.function];
    let mut live_values = HashSet::new();
    let region = function.get_region(ctx);
    for block in region.deref(ctx).iter(ctx) {
        live_values.extend(block.deref(ctx).arguments());
    }
    let ops = crate::aggregates::operations(ctx, &function);
    for &op in &ops {
        live_values.extend(op.deref(ctx).results());
    }
    let entry = function.get_entry_block(ctx);
    let orig_arg_types = body.signature.deref(ctx).arg_types();
    for (pos, &param) in body.parameters.iter().enumerate() {
        let orig_arg_ty = orig_arg_types[pos];
        let arg_ty = entry.deref(ctx).get_argument(pos).get_type(ctx);
        if key
            .args
            .get(pos)
            .copied()
            .flatten()
            .is_some_and(|d| d.bits == 64)
            && is_fp64_carrier(ctx, arg_ty)
        {
            return verify_err!(
                loc,
                LosslessRepresentationErr(format!("64-bit parameter {pos} has f64 carrier"))
            );
        }
        if state.fact(param) == super::Fact::Numeric
            && !orig_arg_ty.deref(ctx).is::<PromiseType>()
            && is_fp64_carrier(ctx, arg_ty)
        {
            return verify_err!(
                loc,
                LosslessRepresentationErr(format!("Fact::Numeric parameter {pos} has f64 carrier"))
            );
        }
        if let Some(&cell) = state.heap_values.get(&param)
            && !orig_arg_ty.deref(ctx).is::<PromiseType>()
        {
            analysis.verify_cell_layout_lossless(ctx, loc.clone(), arg_ty, cell)?;
        }
    }
    let orig_res_types = body.signature.deref(ctx).res_types();
    let actual_res_types = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?
        .deref(ctx)
        .res_types();
    for (pos, res_fact) in state.results.iter().enumerate() {
        let orig_ty = orig_res_types[pos];
        let actual_ty = actual_res_types[pos];
        if (!is_root || super::heap::aggregate(ctx, orig_ty))
            && !orig_ty.deref(ctx).is::<PromiseType>()
        {
            if (res_fact.domain.integer().is_some_and(|d| d.bits == 64)
                || res_fact.domain == super::Fact::Numeric)
                && is_fp64_carrier(ctx, actual_ty)
            {
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(format!(
                        "result {pos} with {:?} has f64 carrier",
                        res_fact.domain
                    ))
                );
            }
            if let Some(cell) = res_fact.heap {
                analysis.verify_cell_layout_lossless(ctx, loc.clone(), actual_ty, cell)?;
            }
        }
    }
    if let Some(mapping) = mapping {
        let values = state.facts.keys().chain(
            state
                .heap_values
                .keys()
                .filter(|value| !state.facts.contains_key(value)),
        );
        for &orig_val in values {
            if body.parameters.contains(&orig_val) {
                continue;
            }
            let mapped = mapping.lookup_value(orig_val).unwrap_or(orig_val);
            if !live_values.contains(&mapped) {
                continue;
            }
            let actual_ty = mapped.get_type(ctx);
            if can_specialize_value(ctx, state, orig_val)
                && state.representation(orig_val).is_some_and(|d| d.bits == 64)
                && !orig_val.get_type(ctx).deref(ctx).is::<PromiseType>()
                && is_fp64_carrier(ctx, actual_ty)
            {
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(
                        "64-bit integer SSA value degraded to f64 carrier".into()
                    )
                );
            }
            if !orig_val.get_type(ctx).deref(ctx).is::<PromiseType>()
                && state.fact(orig_val) == super::Fact::Numeric
                && is_fp64_carrier(ctx, actual_ty)
            {
                let fn_name = function.get_symbol_name(ctx);
                let def_op = orig_val
                    .defining_op()
                    .map(|op| Operation::get_op_dyn(op, ctx).get_opid().to_string())
                    .unwrap_or_else(|| "block_arg".to_string());
                return verify_err!(
                    loc,
                    LosslessRepresentationErr(format!(
                        "Fact::Numeric SSA value ({def_op} in {fn_name}) degraded to f64 carrier"
                    ))
                );
            }
            if let Some(&cell) = state.heap_values.get(&orig_val)
                && !orig_val.get_type(ctx).deref(ctx).is::<PromiseType>()
            {
                analysis.verify_cell_layout_lossless(ctx, loc.clone(), actual_ty, cell)?;
            }
        }
    }
    verify_lossless_function_ops(ctx, function)?;
    Ok(())
}
