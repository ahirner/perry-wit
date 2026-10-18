//! Shared final-use analysis for owned strings and mutable dense arrays.
use crate::aggregates::{functions, operations};
use pliron::{
    basic_block::BasicBlock,
    context::{Context, Ptr},
    irbuild::{
        inserter::Inserter,
        match_rewrite::{MatchRewrite, MatchRewriter, PassWrapper},
        rewriter::Rewriter,
    },
    linked_list::{ContainsLinkedList, LinkedList},
    op::Op,
    operation::Operation,
    pass::{AnalysisManager, Pass, PassResult, Passes},
    result::Result,
    r#type::Typed,
    value::Value,
};
use std::collections::{HashMap, HashSet};

struct Transfer {
    block: Ptr<BasicBlock>,
    argument: Value,
}

struct FunctionCfg {
    name: pliron::identifier::Identifier,
    blocks: Vec<Ptr<BasicBlock>>,
    ops: Vec<Ptr<Operation>>,
    returns: Vec<Ptr<Operation>>,
    incoming: HashMap<Value, Vec<Value>>,
    passed: HashMap<(Ptr<Operation>, Value), Vec<Transfer>>,
    aliases: HashMap<Value, HashSet<Value>>,
}

/// Analyze exclusive sequence headers and their final observations without changing MIR.
/// Direct return summaries start unresolved; fresh allocations prove them monotonically.
fn reuse_candidates(
    root: Ptr<Operation>,
    ctx: &Context,
    live: &crate::liveness::LiveValues,
) -> HashMap<Ptr<Operation>, crate::attrs::ReuseOperands> {
    use crate::{calls::CallOp, views::AggregateViewOp};
    use crate::{
        collections::{
            ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
            ConcatOp, ConcatReuseOp, StringConstOp, StringLenOp,
        },
        control::ReturnOp,
        intrinsics::Intrinsic,
        scalar::CompareOp,
        types::{ArrayType, StringType},
    };
    use pliron::builtin::op_interfaces::{
        BranchOpInterface, OneRegionInterface, SymbolOpInterface,
    };
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
            || ((Operation::get_op::<ArrayLenOp>(op, ctx).is_some()
                || Operation::get_op::<ArrayGetOp>(op, ctx).is_some()
                || Operation::get_op::<ArrayAtOp>(op, ctx).is_some()
                || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
                || Operation::get_op::<ArraySetOp>(op, ctx).is_some())
                && op.deref(ctx).get_operand(0) == value
                && !op.deref(ctx).operands().skip(1).any(|v| v == value))
            || Operation::get_op::<StringLenOp>(op, ctx).is_some()
            || Operation::get_op::<CompareOp>(op, ctx).is_some()
            || Operation::get_op::<ReturnOp>(op, ctx).is_some()
            || matches!(
                crate::intrinsics::intrinsic_of(ctx, op),
                Some(
                    Intrinsic::StringCharAt
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
    let cfgs: Vec<FunctionCfg> = functions(root, ctx)
        .into_iter()
        .map(|function| {
            let blocks: Vec<Ptr<BasicBlock>> =
                function.get_region(ctx).deref(ctx).iter(ctx).collect();
            let ops = operations(ctx, &function);
            let mut incoming: HashMap<Value, Vec<Value>> = HashMap::new();
            let mut passed: HashMap<(Ptr<Operation>, Value), Vec<Transfer>> = HashMap::new();
            for &block in &blocks {
                let Some(terminator) = block.deref(ctx).get_terminator(ctx) else {
                    continue;
                };
                let dests: Vec<Ptr<BasicBlock>> = terminator.deref(ctx).successors().collect();
                let dyn_op = Operation::get_op_dyn(terminator, ctx);
                if let Some(branch) = pliron::op::op_cast::<dyn BranchOpInterface>(dyn_op.as_ref())
                {
                    for (succ, &dest) in dests.iter().enumerate() {
                        for (index, value) in
                            branch.successor_operands(ctx, succ).into_iter().enumerate()
                        {
                            let argument = dest.deref(ctx).get_argument(index);
                            incoming.entry(argument).or_default().push(value);
                            passed
                                .entry((terminator, value))
                                .or_default()
                                .push(Transfer {
                                    block: dest,
                                    argument,
                                });
                        }
                    }
                }
            }
            let mut aliases: HashMap<Value, HashSet<Value>> = HashMap::new();
            for &op in &ops {
                if Operation::get_op::<AggregateViewOp>(op, ctx).is_some() {
                    let (source, view) =
                        (op.deref(ctx).get_operand(0), op.deref(ctx).get_result(0));
                    if is_sequence(ctx, view) {
                        aliases.entry(source).or_default().insert(view);
                        aliases.entry(view).or_default().insert(source);
                    }
                }
            }
            let returns = ops
                .iter()
                .copied()
                .filter(|&op| Operation::get_op::<ReturnOp>(op, ctx).is_some())
                .collect();
            FunctionCfg {
                name: function.get_symbol_name(ctx),
                blocks,
                ops,
                returns,
                incoming,
                passed,
                aliases,
            }
        })
        .collect();
    let alias_set = |aliases: &HashMap<Value, HashSet<Value>>, value: Value| {
        let mut seen = HashSet::new();
        let mut work = vec![value];
        while let Some(next) = work.pop() {
            if seen.insert(next) {
                work.extend(aliases.get(&next).into_iter().flatten().copied());
            }
        }
        seen
    };
    let mut fresh_returns = HashSet::new();
    let mut owned_by_fn = vec![HashSet::new(); cfgs.len()];
    loop {
        let mut gained = false;
        for (cfg, owned) in cfgs.iter().zip(owned_by_fn.iter_mut()) {
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
            for &block in &cfg.blocks {
                for argument in block.deref(ctx).arguments() {
                    if cfg.incoming.contains_key(&argument) && is_sequence(ctx, argument) {
                        owned.insert(argument);
                    }
                }
            }
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
                            let Some(arguments) = cfg.passed.get(&(user, value)) else {
                                return !alias_set(&cfg.aliases, value)
                                    .iter()
                                    .any(|&alias| live.after(user).contains(&alias));
                            };
                            let aliases = alias_set(&cfg.aliases, value);
                            arguments.len() == 1
                                && owned.contains(&arguments[0].argument)
                                && aliases
                                    .iter()
                                    .filter_map(|&alias| cfg.passed.get(&(user, alias)))
                                    .map(Vec::len)
                                    .sum::<usize>()
                                    == 1
                                && !aliases
                                    .iter()
                                    .any(|&alias| live.on_entry(arguments[0].block, alias))
                        })
                    })
                    .collect();
                if removed.is_empty() {
                    break;
                }
                for value in removed {
                    for alias in alias_set(&cfg.aliases, value) {
                        owned.remove(&alias);
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
    let mut candidates = HashMap::new();
    for (cfg, owned) in cfgs.iter().zip(owned_by_fn.iter()) {
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
                    if !(array || index == 0) || !owned.contains(&value) {
                        return None;
                    }
                    let aliases = alias_set(&cfg.aliases, value);
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
            candidates.insert(op, reuse);
        }
    }
    candidates
}

/// Fuse unobserved array combinations while their snapshots remain unchanged.
fn fuse_arrays(root: Ptr<Operation>, ctx: &mut Context) -> bool {
    use crate::{collections::ConcatOp, exceptions::MayThrowInterface, types::ArrayType};
    use pliron::{op::op_cast, opts::dce::SideEffects};
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
                    Operation::get_op::<ConcatOp>(prior, ctx).is_some()
                        && source.uses(ctx).len() == 1
                        && prior.deref(ctx).get_parent_block() == op.deref(ctx).get_parent_block()
                });
                let prior = prior.filter(|&prior| {
                    let mut next = prior.deref(ctx).get_next();
                    while let Some(intermediate) = next {
                        if intermediate == op {
                            return true;
                        }
                        let operation = Operation::get_op_dyn(intermediate, ctx);
                        if op_cast::<dyn MayThrowInterface>(operation.as_ref()).is_some()
                            || op_cast::<dyn SideEffects>(operation.as_ref())
                                .is_none_or(|effect| effect.has_side_effects(ctx))
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

struct ReusableSequences {
    candidates: HashMap<Ptr<Operation>, crate::attrs::ReuseOperands>,
}

struct ReusePass;

impl Pass for ReusePass {
    fn name(&self) -> &str {
        "owned-append"
    }
    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let candidates = {
            let live = analyses.get_analysis::<crate::liveness::LiveValues>(root, ctx)?;
            reuse_candidates(root, ctx, &live)
        };
        PassWrapper::new("owned-append", ReusableSequences { candidates }).run(root, ctx, analyses)
    }
}

impl MatchRewrite for ReusableSequences {
    fn r#match(&mut self, ctx: &Context, op: Ptr<Operation>) -> bool {
        self.candidates.contains_key(&op)
            && Operation::get_op::<crate::collections::ConcatOp>(op, ctx).is_some()
    }
    fn rewrite(
        &mut self,
        ctx: &mut Context,
        rewriter: &mut MatchRewriter,
        op: Ptr<Operation>,
    ) -> Result<()> {
        let operands = op.deref(ctx).operands().collect();
        let reuse = self
            .candidates
            .remove(&op)
            .expect("matched reuse candidate");
        let append =
            crate::collections::ConcatReuseOp::from_sources(ctx, operands, reuse).get_operation();
        rewriter.insert_operation(ctx, append);
        rewriter.replace_operation(ctx, op, append);
        Ok(())
    }
}

struct ArrayFusion;

impl Pass for ArrayFusion {
    fn name(&self) -> &str {
        "fuse-array-concats"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        _: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let mut result = PassResult::default();
        result.ir_changed = fuse_arrays(root, ctx).into();
        Ok(result)
    }
}

pub(crate) struct AppendPass;

impl Pass for AppendPass {
    fn name(&self) -> &str {
        "owned-append"
    }

    fn run(
        &mut self,
        root: Ptr<Operation>,
        ctx: &mut Context,
        analyses: &mut AnalysisManager,
    ) -> Result<PassResult> {
        let mut passes = Passes::default();
        passes.add_pass(ArrayFusion);
        passes.add_pass(ReusePass);
        passes.run(root, ctx, analyses)
    }
}
