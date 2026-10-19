//! Representation rewriting after ProgramFacts has converged.
use super::{
    ArgumentDomain, Constraint, Integer, ProgramFacts, SPECIALIZATION, WIT_SIGNATURE,
    comparison_domain, literal,
};
use crate::{
    attrs::BinaryKind,
    calls::{AsyncCallOp, AwaitOp, CallOp},
    collections::{ArrayAtOp, ArrayGetOp},
    control::ReturnOp,
    heap::FieldGetOp,
    locals::LoadOp,
    optional::{OptionalCastOp, ToNumberOp, UnwrapOp, WrapOp},
    scalar::{BinaryOp, CompareOp, NumericCastOp},
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, IntegerAttr, TypeAttr},
        op_interfaces::{
            BranchOpInterface, OneRegionInterface, OneResultInterface, SymbolOpInterface,
        },
        ops::{ConstantOp, FuncOp},
        type_interfaces::FunctionTypeInterface,
        types::{FunctionType, IntegerType},
    },
    context::{Context, Ptr},
    identifier::{Identifier, Legaliser},
    irbuild::{
        cloning::{IrMapping, clone_operation},
        inserter::{Inserter, OpInsertionPoint},
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    operation::Operation,
    pass::{AnalysisManager, PassResult},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    utils::apint::{APInt, bw},
};
use std::collections::{HashMap, HashSet};

mod operands;
mod plan;
use plan::{BodyPlan, ContextPlan};

pub(crate) fn specialize(
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
    result.ir_changed = (specializations != 0).into();
    Ok(result)
}

/// One emitted function body, shared by every context whose `BodyPlan` matches.
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
    // Projections keep the representation of what they read.
    if crate::is_any::<(FieldGetOp, ArrayGetOp, ArrayAtOp, LoadOp, AwaitOp)>(ctx, op)
        || crate::is_any::<(WrapOp, UnwrapOp, OptionalCastOp, ToNumberOp)>(ctx, op)
    {
        return false;
    }
    crate::intrinsics::intrinsic_of(ctx, op).is_none_or(|intrinsic| {
        integral_intrinsic(ctx, op, intrinsic, |value| {
            state.representation(value).is_some()
        })
    })
}

/// Whether intrinsic `op` has an integer result: always for BigInt negation, `parseInt` and
/// proven byte reads, and for the integer-preserving math services when `integral` holds for
/// every operand.
pub(in crate::facts) fn integral_intrinsic(
    ctx: &Context,
    op: Ptr<Operation>,
    intrinsic: crate::intrinsics::Intrinsic,
    integral: impl Fn(pliron::value::Value) -> bool,
) -> bool {
    use crate::intrinsics::Intrinsic;
    match intrinsic {
        Intrinsic::BigIntNegate | Intrinsic::BytesAt | Intrinsic::ParseInt => true,
        _ if intrinsic.preserves_integers() => op.deref(ctx).operands().all(integral),
        _ => false,
    }
}

fn rewrite(ctx: &mut Context, analysis: &ProgramFacts) -> Result<usize> {
    let mut rewriter = IRRewriter::<DummyListener>::default();
    let root_ids: HashSet<usize> = analysis.roots.iter().map(|&(_, id)| id).collect();
    // Contexts called, transitively, from the specialized roots.
    let reachable = crate::reachable(root_ids.iter().copied(), |id| {
        analysis.states[id].calls.values().copied()
    });
    let plans = reachable
        .iter()
        .map(|&id| {
            ContextPlan::new(ctx, analysis, id, root_ids.contains(&id)).map(|plan| (id, plan))
        })
        .collect::<Result<HashMap<_, _>>>()?;
    let changed = changed_contexts(analysis, &plans);
    let mut implementations = Implementations::default();
    let mut coalesced_map: HashMap<&BodyPlan, usize> = HashMap::new();
    for (id, key) in analysis
        .keys
        .iter()
        .enumerate()
        .filter(|(id, _)| reachable.contains(id))
    {
        let body = &analysis.bodies[&key.function];
        let plan = &plans[&id];
        let ckey = (changed.contains(&id) && !root_ids.contains(&id)).then_some(&plan.body);
        if let Some(&impl_idx) = ckey.and_then(|ckey| coalesced_map.get(ckey)) {
            widen_arguments(ctx, &mut implementations.unique[impl_idx], &key.args);
            implementations.of.insert(id, impl_idx);
            continue;
        }
        let mut implementation = Implementation {
            function: body.function,
            mapping: IrMapping::new(),
            arg_domains: key.args.iter().map(|a| a.integer()).collect(),
            arguments: if changed.contains(&id) {
                plan.arguments.clone()
            } else {
                body.signature.deref(ctx).arg_types()
            },
            results: if changed.contains(&id) {
                plan.body.results.clone()
            } else {
                body.signature.deref(ctx).res_types()
            },
            primary_context: id,
        };
        if changed.contains(&id) {
            clone_implementation(ctx, analysis, &mut rewriter, &mut implementation);
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
                &plans[&implementation.primary_context],
            )?;
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
                .get(&original.get_operation())
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

/// Reachable specialized contexts whose types change, plus their specialized callers, which
/// must call the rewritten clone.
fn changed_contexts(
    analysis: &ProgramFacts,
    plans: &HashMap<usize, ContextPlan>,
) -> HashSet<usize> {
    let roots = plans
        .iter()
        .filter_map(|(&id, plan)| (analysis.keys[id].specialize && plan.changed).then_some(id));
    crate::reachable(roots, |id| {
        analysis.states[id]
            .callers
            .iter()
            .filter_map(|&(caller, _)| {
                (analysis.keys[caller].specialize && plans.contains_key(&caller)).then_some(caller)
            })
    })
}

/// Widens a coalesced implementation's integer arguments to also accept `args`.
fn widen_arguments(ctx: &mut Context, imp: &mut Implementation, args: &[ArgumentDomain]) {
    let mut widened = false;
    for (pos, (&new_dom, cur_dom)) in args.iter().zip(imp.arg_domains.iter_mut()).enumerate() {
        if let (Some(new_int), Some(cur_int)) = (new_dom.integer(), *cur_dom) {
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
    let inner = crate::optional::present_type(ctx, ty);
    inner.deref(ctx).is::<pliron::builtin::types::FP64Type>()
}

/// Verifies that no specialized function in `root` backs a `Fact::Numeric` or 64-bit integer
/// value or aggregate slot with a lossy `builtin.fp64` carrier.
pub(crate) fn verify_lossless_module(
    root: Ptr<Operation>,
    ctx: &Context,
    analysis: &ProgramFacts,
) -> Result<()> {
    let funcs: HashMap<Identifier, FuncOp> = crate::aggregates::functions(root, ctx)
        .into_iter()
        .map(|f| (f.get_symbol_name(ctx), f))
        .collect();
    let root_ids: HashSet<usize> = analysis.roots.iter().map(|&(_, id)| id).collect();
    // A root or its specialization missing from the module would go unverified.
    let function = |name: &Identifier| {
        funcs.get(name).copied().map_or_else(
            || {
                pliron::verify_err!(
                    pliron::location::Location::Unknown,
                    LosslessRepresentationErr(format!("no function `{name}` to verify"))
                )
            },
            Ok,
        )
    };
    for &(_, id) in &analysis.roots {
        let original = function(&analysis.keys[id].function)?;
        let func = function(&super::specialized_implementation(ctx, &original))?;
        verify_lossless_context(ctx, analysis, id, func, None, root_ids.contains(&id))?;
    }
    Ok(())
}

/// Verifies that no active function in `root` contains an operation whose lossless
/// (`perry.numeric` or 64-bit integer) result or field/element projection was degraded
/// to a `builtin.fp64` carrier after all MIR passes complete.
pub(crate) fn verify_lossless_operations(root: Ptr<Operation>, ctx: &Context) -> Result<()> {
    let functions = crate::aggregates::functions(root, ctx);
    let by_name: HashMap<Identifier, FuncOp> = functions
        .iter()
        .map(|&f| (f.get_symbol_name(ctx), f))
        .collect();
    let entries = functions.iter().filter_map(|func| {
        (super::wit_signature(ctx, func).is_some() || super::is_entry(ctx, func))
            .then(|| {
                by_name
                    .get(&super::specialized_implementation(ctx, func))
                    .copied()
            })
            .flatten()
    });
    let active = crate::reachable(entries, |func| {
        crate::aggregates::operations(ctx, &func)
            .into_iter()
            .filter_map(|op| by_name.get(&crate::calls::callee(ctx, op)?).copied())
    });
    active
        .into_iter()
        .try_for_each(|func| verify_lossless_function_ops(ctx, func))
}

fn is_lossless_type(ctx: &Context, ty: TypeHandle) -> bool {
    let inner = crate::optional::present_type(ctx, ty);
    inner.deref(ctx).is::<crate::types::NumericType>()
        || Integer::from_type(ctx, inner).is_some_and(|d| d.bits == 64)
}

fn verify_lossless_function_ops(ctx: &Context, function: FuncOp) -> Result<()> {
    for op in crate::aggregates::operations(ctx, &function) {
        if let Some(projection) = lossless_projection(ctx, op)
            && is_fp64_carrier(ctx, op.deref(ctx).get_result(0).get_type(ctx))
        {
            return pliron::verify_err!(
                function.loc(ctx).clone(),
                LosslessRepresentationErr(format!("{projection} degraded to f64 carrier"))
            );
        }
    }
    Ok(())
}

/// What `op`'s result projects when that is lossless: a `parseInt` result, a field or element
/// of a lossless type, or a lossless value moved into or out of an optional.
fn lossless_projection(ctx: &Context, op: Ptr<Operation>) -> Option<String> {
    let operand = || op.deref(ctx).get_operand(0).get_type(ctx);
    if crate::intrinsics::intrinsic_of(ctx, op) == Some(crate::intrinsics::Intrinsic::ParseInt) {
        return Some("ParseInt result".into());
    }
    let (projected, projection) =
        if let Some(get) = Operation::get_op::<crate::heap::FieldGetOp>(op, ctx) {
            let field = get.field(ctx);
            let ty = operand()
                .deref(ctx)
                .downcast_ref::<crate::types::RecordType>()?
                .field(&field)?
                .1;
            (ty, format!("field_get of lossless field `{field}`"))
        } else if Operation::get_op::<crate::collections::ArrayGetOp>(op, ctx).is_some()
            || Operation::get_op::<crate::collections::ArrayAtOp>(op, ctx).is_some()
        {
            let array = operand();
            let element = array
                .deref(ctx)
                .downcast_ref::<crate::types::ArrayType>()?
                .element();
            (element, "array element load of lossless element".into())
        } else if Operation::get_op::<crate::optional::UnwrapOp>(op, ctx).is_some() {
            (operand(), "optional unwrap of lossless value".into())
        } else if Operation::get_op::<crate::optional::WrapOp>(op, ctx).is_some() {
            (operand(), "optional wrap of lossless value".into())
        } else {
            return None;
        };
    is_lossless_type(ctx, projected).then_some(projection)
}

fn verify_lossless_context(
    ctx: &Context,
    analysis: &ProgramFacts,
    id: usize,
    function: FuncOp,
    mapping: Option<&IrMapping>,
    is_root: bool,
) -> Result<()> {
    let loc = function.loc(ctx);
    let fail = |message: String| -> Result<()> {
        pliron::verify_err!(loc.clone(), LosslessRepresentationErr(message))
    };
    let key = &analysis.keys[id];
    let state = &analysis.states[id];
    let body = &analysis.bodies[&key.function];
    let mut live_values = HashSet::new();
    for block in function.get_region(ctx).deref(ctx).iter(ctx) {
        live_values.extend(block.deref(ctx).arguments());
    }
    for op in crate::aggregates::operations(ctx, &function) {
        live_values.extend(op.deref(ctx).results());
    }
    let entry = function.get_entry_block(ctx);
    let orig_arg_types = body.signature.deref(ctx).arg_types();
    for (pos, &param) in body.parameters.iter().enumerate() {
        let orig_arg_ty = orig_arg_types[pos];
        let arg_ty = entry.deref(ctx).get_argument(pos).get_type(ctx);
        let wide = key
            .args
            .get(pos)
            .and_then(|a| a.integer())
            .is_some_and(|d| d.bits == 64);
        if wide && is_fp64_carrier(ctx, arg_ty) {
            return fail(format!("64-bit parameter {pos} has f64 carrier"));
        }
        if state.fact(param) == super::Fact::Numeric
            && !orig_arg_ty.deref(ctx).is::<PromiseType>()
            && is_fp64_carrier(ctx, arg_ty)
        {
            return fail(format!("Fact::Numeric parameter {pos} has f64 carrier"));
        }
        if let Some(&cell) = state.heap_values.get(&param)
            && !orig_arg_ty.deref(ctx).is::<PromiseType>()
        {
            analysis.verify_cell_layout_lossless(ctx, &loc, arg_ty, cell)?;
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
            if res_fact.facts.domain.needs_lossless_64() && is_fp64_carrier(ctx, actual_ty) {
                let domain = res_fact.facts.domain;
                return fail(format!("result {pos} with {domain:?} has f64 carrier"));
            }
            if let Some(cell) = res_fact.heap {
                analysis.verify_cell_layout_lossless(ctx, &loc, actual_ty, cell)?;
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
                return fail("64-bit integer SSA value degraded to f64 carrier".into());
            }
            if !orig_val.get_type(ctx).deref(ctx).is::<PromiseType>()
                && state.fact(orig_val) == super::Fact::Numeric
                && is_fp64_carrier(ctx, actual_ty)
            {
                let fn_name = function.get_symbol_name(ctx);
                let def_op = orig_val.defining_op().map_or_else(
                    || "block_arg".to_string(),
                    |op| Operation::get_opid(op, ctx).to_string(),
                );
                return fail(format!(
                    "Fact::Numeric SSA value ({def_op} in {fn_name}) degraded to f64 carrier"
                ));
            }
            if let Some(&cell) = state.heap_values.get(&orig_val)
                && !orig_val.get_type(ctx).deref(ctx).is::<PromiseType>()
            {
                analysis.verify_cell_layout_lossless(ctx, &loc, actual_ty, cell)?;
            }
        }
    }
    verify_lossless_function_ops(ctx, function)?;
    Ok(())
}
