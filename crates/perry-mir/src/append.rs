//! Shared final-use analysis for owned strings and mutable dense arrays.
use crate::aggregates::{functions, operations};
use pliron::{
    context::{Context, Ptr},
    irbuild::{
        listener::DummyListener,
        rewriter::{IRRewriter, Rewriter},
    },
    linked_list::LinkedList,
    op::Op,
    operation::Operation,
    pass::{AnalysisManager, PassResult},
    result::Result,
    r#type::Typed,
    utils::union_find::UnionFind,
    value::Value,
};
use std::collections::{HashMap, HashSet};

struct FunctionCfg {
    name: pliron::identifier::Identifier,
    ops: Vec<Ptr<Operation>>,
    returns: Vec<Ptr<Operation>>,
    incoming: HashMap<Value, Vec<Value>>,
    aliases: UnionFind<Value>,
    owned: HashSet<Value>,
}

/// Analyze exclusive sequence headers and their final observations across the module.
/// Direct return summaries start unresolved and converge monotonically as fresh allocations prove exclusivity.
fn reuse_candidates(
    root: Ptr<Operation>,
    ctx: &Context,
    live: &crate::liveness::LiveValues,
) -> Vec<(Ptr<Operation>, crate::attrs::ReuseOperands)> {
    use crate::{calls::CallOp, views::AggregateViewOp};
    use crate::{
        collections::{
            ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
            ArrayStoreOp, ConcatOp, ConcatReuseOp, StringConstOp, StringLenOp,
        },
        control::ReturnOp,
        intrinsics::Intrinsic,
        scalar::CompareOp,
        types::{ArrayType, StringType},
    };
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    let is_sequence = |ctx: &Context, v: Value| {
        let ty = v.get_type(ctx).deref(ctx);
        ty.is::<StringType>() || ty.is::<ArrayType>()
    };
    let is_concat = |ctx: &Context, op: Ptr<Operation>| {
        Operation::get_op::<ConcatOp>(op, ctx).is_some()
            || Operation::get_op::<ConcatReuseOp>(op, ctx).is_some()
    };
    let is_constant = |ctx: &Context, v: Value| {
        v.defining_op()
            .is_some_and(|op| Operation::get_op::<StringConstOp>(op, ctx).is_some())
    };
    let reads = |ctx: &Context, op: Ptr<Operation>, value: Value| {
        is_concat(ctx, op)
            || Operation::get_op::<AggregateViewOp>(op, ctx).is_some()
            || Operation::get_op::<ArrayExtendOp>(op, ctx).is_some()
            || (crate::is_any::<(
                ArrayLenOp,
                ArrayGetOp,
                ArrayAtOp,
                ArrayPushOp,
                ArraySetOp,
                ArrayStoreOp,
            )>(ctx, op)
                && op.deref(ctx).get_operand(0) == value
                && !op.deref(ctx).operands().skip(1).any(|v| v == value))
            || Operation::get_op::<StringLenOp>(op, ctx).is_some()
            || Operation::get_op::<CompareOp>(op, ctx).is_some()
            || Operation::get_op::<ReturnOp>(op, ctx).is_some()
            || matches!(
                crate::intrinsics::intrinsic_of(ctx, op),
                Some(
                    Intrinsic::StringCharAt
                        | Intrinsic::StringCharCodeAt
                        | Intrinsic::StringCharAtIndex
                        | Intrinsic::StringIndex
                        | Intrinsic::StringCodePointAt
                        | Intrinsic::StringSlice
                        | Intrinsic::StringSubstring
                        | Intrinsic::StringIndexOf
                        | Intrinsic::StringLastIndexOf
                        | Intrinsic::StringStartsWith
                        | Intrinsic::StringEndsWith
                )
            )
    };
    let mut cfgs: Vec<FunctionCfg> = functions(root, ctx)
        .into_iter()
        .map(|function| {
            let ops = operations(ctx, &function);
            let mut incoming: HashMap<Value, Vec<Value>> = HashMap::new();
            let mut aliases = UnionFind::default();
            let mut returns = Vec::new();
            for &op in &ops {
                for (position, argument) in pliron_control::forwarded_arguments(ctx, op) {
                    let input = op.deref(ctx).get_operand(position);
                    incoming.entry(argument).or_default().push(input);
                }
                if Operation::get_op::<AggregateViewOp>(op, ctx).is_some() {
                    let (source, view) =
                        (op.deref(ctx).get_operand(0), op.deref(ctx).get_result(0));
                    if is_sequence(ctx, view) {
                        aliases.union(source, view);
                    }
                } else if Operation::get_op::<ReturnOp>(op, ctx).is_some() {
                    returns.push(op);
                }
            }
            FunctionCfg {
                name: function.get_symbol_name(ctx),
                ops,
                returns,
                incoming,
                owned: HashSet::new(),
                aliases,
            }
        })
        .collect();
    let mut fresh_returns = HashSet::new();
    loop {
        let mut gained = false;
        for cfg in &mut cfgs {
            let owned = &mut cfg.owned;
            owned.clear();
            let fresh_header = |ctx: &Context, op: Ptr<Operation>| {
                Operation::get_op::<CallOp>(op, ctx)
                    .is_some_and(|call| fresh_returns.contains(&call.callee(ctx)))
                    || is_concat(ctx, op)
                    || Operation::get_op::<ArrayNewOp>(op, ctx).is_some()
                    || Operation::get_op::<crate::collections::ConcatSegmentsOp>(op, ctx).is_some()
                    || matches!(
                        crate::intrinsics::intrinsic_of(ctx, op),
                        Some(
                            Intrinsic::StringCharAt
                                | Intrinsic::StringCharAtIndex
                                | Intrinsic::StringSlice
                                | Intrinsic::StringSubstring
                        )
                    )
            };
            owned.extend(
                cfg.incoming
                    .keys()
                    .copied()
                    .filter(|&arg| is_sequence(ctx, arg)),
            );
            for &op in &cfg.ops {
                if fresh_header(ctx, op)
                    || (Operation::get_op::<AggregateViewOp>(op, ctx).is_some()
                        && is_sequence(ctx, op.deref(ctx).get_result(0)))
                {
                    owned.insert(op.deref(ctx).get_result(0));
                }
            }
            loop {
                let removed: Vec<Value> = owned
                    .iter()
                    .copied()
                    .filter(|&value| {
                        if value.defining_op().is_none()
                            && cfg.incoming.get(&value).is_none_or(|values| {
                                !values
                                    .iter()
                                    .all(|v| owned.contains(v) || is_constant(ctx, *v))
                            })
                        {
                            return true;
                        }
                        if let Some(op) = value.defining_op()
                            && Operation::get_op::<AggregateViewOp>(op, ctx).is_some()
                            && !owned.contains(&op.deref(ctx).get_operand(0))
                        {
                            return true;
                        }
                        let uses = value.uses(ctx);
                        !uses.iter().all(|use_| {
                            let user = use_.user_op();
                            if reads(ctx, user, value) {
                                return true;
                            }
                            // Transfer the header only after its final direct observation.
                            let aliases = cfg.aliases.set_members(value);
                            let Some(edge) =
                                pliron_control::forwarded_operand(ctx, user, use_.find_index(ctx))
                            else {
                                return !aliases
                                    .iter()
                                    .any(|alias| live.after(user).contains(alias));
                            };
                            owned.contains(&edge.argument)
                                && edge
                                    .arm_operands
                                    .filter(|&i| aliases.contains(&user.deref(ctx).get_operand(i)))
                                    .count()
                                    == 1
                                && !aliases
                                    .iter()
                                    .any(|&alias| live.on_entry(edge.block, alias))
                        })
                    })
                    .collect();
                if removed.is_empty() {
                    break;
                }
                for value in removed {
                    for alias in cfg.aliases.set_members(value) {
                        owned.remove(alias);
                    }
                }
            }
            if !cfg.returns.is_empty()
                && cfg.returns.iter().all(|&op| {
                    op.deref(ctx).get_num_operands() == 1
                        && owned.contains(&op.deref(ctx).get_operand(0))
                })
            {
                gained |= fresh_returns.insert(cfg.name.clone());
            }
        }
        if !gained {
            break;
        }
    }
    let mut candidates = Vec::new();
    for cfg in &mut cfgs {
        for &op in &cfg.ops {
            if Operation::get_op::<ConcatOp>(op, ctx).is_none() {
                continue;
            }
            let operands: Vec<_> = op.deref(ctx).operands().collect();
            let array = operands[0].get_type(ctx).deref(ctx).is::<ArrayType>();
            let indices = operands
                .iter()
                .enumerate()
                .filter_map(|(index, &value)| {
                    if !(array || index == 0) || !cfg.owned.contains(&value) {
                        return None;
                    }
                    let aliases = cfg.aliases.set_members(value);
                    (operands
                        .iter()
                        .filter(|&&other| aliases.contains(&other))
                        .count()
                        == 1
                        && !aliases.iter().any(|&alias| live.after(op).contains(&alias)))
                    .then_some(index as u32)
                })
                .collect::<Vec<_>>();
            if indices.is_empty() {
                continue;
            }
            let reuse = crate::attrs::ReuseOperands(indices);
            candidates.push((op, reuse));
        }
    }
    candidates
}

/// Fuse unobserved array combinations while their snapshots remain unchanged.
pub(crate) fn fuse_arrays(root: Ptr<Operation>, ctx: &mut Context) -> bool {
    use crate::{collections::ConcatOp, types::ArrayType};
    use pliron::opts::dce::has_side_effects;
    let mut changed = false;
    for function in functions(root, ctx) {
        for op in operations(ctx, &function) {
            if Operation::get_op::<ConcatOp>(op, ctx).is_none()
                || !op
                    .deref(ctx)
                    .get_result(0)
                    .get_type(ctx)
                    .deref(ctx)
                    .is::<ArrayType>()
            {
                continue;
            }
            let mut sources = Vec::new();
            let mut erased = Vec::new();
            for source in op.deref(ctx).operands() {
                let prior = source.defining_op().filter(|&prior| {
                    if Operation::get_op::<ConcatOp>(prior, ctx).is_none()
                        || source.uses(ctx).len() != 1
                        || prior.deref(ctx).get_parent_block() != op.deref(ctx).get_parent_block()
                    {
                        return false;
                    }
                    let mut next = prior.deref(ctx).get_next();
                    while let Some(intermediate) = next {
                        if intermediate == op {
                            return true;
                        }
                        if crate::semantics::exception(ctx, intermediate).is_some()
                            || has_side_effects(intermediate, ctx)
                        {
                            return false;
                        }
                        next = intermediate.deref(ctx).get_next();
                    }
                    false
                });
                if let Some(prior) = prior {
                    sources.extend(prior.deref(ctx).operands());
                    erased.push(prior);
                } else {
                    sources.push(source);
                }
            }
            if erased.is_empty() {
                continue;
            }
            changed = true;
            let combined = ConcatOp::from_sources(ctx, sources).get_operation();
            combined.insert_before(ctx, op);
            let (old, new) = (
                op.deref(ctx).get_result(0),
                combined.deref(ctx).get_result(0),
            );
            old.replace_all_uses_with(ctx, &new);
            Operation::erase(op, ctx);
            for prior in erased {
                Operation::erase(prior, ctx);
            }
        }
    }
    changed
}

pub(crate) fn reuse(
    root: Ptr<Operation>,
    ctx: &mut Context,
    analyses: &mut AnalysisManager,
) -> Result<PassResult> {
    let live = analyses.get_analysis::<crate::liveness::LiveValues>(root, ctx)?;
    let candidates = reuse_candidates(root, ctx, &live);
    let mut result = PassResult::default();
    result.ir_changed = (!candidates.is_empty()).into();
    let mut rewriter = IRRewriter::<DummyListener>::default();
    for (op, reuse) in candidates {
        let operands = op.deref(ctx).operands().collect();
        let append =
            crate::collections::ConcatReuseOp::from_sources(ctx, operands, reuse).get_operation();
        append.insert_before(ctx, op);
        rewriter.replace_operation(ctx, op, append);
    }
    Ok(result)
}
