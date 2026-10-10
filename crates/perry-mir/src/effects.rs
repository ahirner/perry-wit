//! Conservative interprocedural allocation and reference-escape summaries.
use crate::{
    calls::{AsyncCallOp, CallOp, TimerOp, is_async},
    collections::{ArrayNewOp, ArrayPushOp, ArraySetOp, StringConcatOp},
    control::ReturnOp,
    exceptions::{CatchOp, ErrorNewOp, RaiseOp},
    heap::{AllocOp, FieldSetOp},
    intrinsics::{
        IntrinsicOp, JsonStringifyOp, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp,
    },
};
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::op_cast,
    operation::Operation,
    r#type::Typed,
    value::Value,
};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub allocates: bool,
    /// The compiled body contains a safe point, including those in direct callees.
    pub may_collect: bool,
    pub may_suspend: bool,
    pub may_reenter: bool,
    pub repeated: bool,
    pub unknown_helper_bound: bool,
    /// Parameters returned, captured by an async call, or stored through an aggregate.
    pub escapes: BTreeSet<usize>,
}

/// Allocation includes hidden temporaries and exception construction inside helpers.
pub fn allocates(ctx: &Context, op: Ptr<Operation>) -> bool {
    Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
        || Operation::get_op::<AllocOp>(op, ctx).is_some()
        || Operation::get_op::<crate::variants::VariantNewOp>(op, ctx).is_some()
        || Operation::get_op::<ArrayNewOp>(op, ctx).is_some()
        || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
        || Operation::get_op::<ArraySetOp>(op, ctx).is_some()
        || Operation::get_op::<StringConcatOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::StringAppendOp>(op, ctx).is_some()
        || Operation::get_op::<TimerOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseResolvedOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseRejectedOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseCombinatorOp>(op, ctx).is_some()
        || Operation::get_op::<JsonStringifyOp>(op, ctx).is_some()
        || Operation::get_op::<ErrorNewOp>(op, ctx).is_some()
        || Operation::get_op::<RaiseOp>(op, ctx).is_some()
        || Operation::get_op::<CatchOp>(op, ctx).is_some()
        || Operation::get_op::<crate::exceptions::ExceptionMessageOp>(op, ctx).is_some()
        || Operation::get_op::<IntrinsicOp>(op, ctx)
            .and_then(|i| i.intrinsic(ctx))
            .is_some_and(|i| i.may_allocate())
}

fn callee(ctx: &Context, op: Ptr<Operation>) -> Option<Identifier> {
    Operation::get_op::<CallOp>(op, ctx)
        .map(|c| c.callee(ctx))
        .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|c| c.callee(ctx)))
}

fn cyclic<N: Copy + Eq + std::hash::Hash>(
    nodes: impl IntoIterator<Item = N>,
    successors: impl Fn(N) -> Vec<N>,
) -> bool {
    fn visit<N: Copy + Eq + std::hash::Hash>(
        node: N,
        successors: &impl Fn(N) -> Vec<N>,
        active: &mut HashSet<N>,
        done: &mut HashSet<N>,
    ) -> bool {
        if done.contains(&node) {
            return false;
        }
        if !active.insert(node) {
            return true;
        }
        if successors(node)
            .into_iter()
            .any(|next| visit(next, successors, active, done))
        {
            return true;
        }
        active.remove(&node);
        done.insert(node);
        false
    }
    let (mut active, mut done) = (HashSet::new(), HashSet::new());
    nodes
        .into_iter()
        .any(|node| visit(node, &successors, &mut active, &mut done))
}

/// Fixed point over direct calls. Unknown bounds propagate to every caller; recursion and
/// CFG cycles never establish bounded task allocation. Escape provenance follows SSA joins,
/// aggregates and calls. Scalar field reads do not retain their containing object.
pub fn summarize(ctx: &Context, functions: &[FuncOp]) -> HashMap<Identifier, Summary> {
    let names: Vec<_> = functions.iter().map(|f| f.get_symbol_name(ctx)).collect();
    let mut summaries: HashMap<_, _> = names
        .iter()
        .cloned()
        .map(|name| (name, Summary::default()))
        .collect();
    let bodies: Vec<Vec<_>> = functions
        .iter()
        .map(|function| {
            function
                .get_region(ctx)
                .deref(ctx)
                .iter(ctx)
                .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
                .collect()
        })
        .collect();
    let call_graph: Vec<Vec<_>> = bodies
        .iter()
        .map(|body| {
            body.iter()
                .filter_map(|&op| {
                    callee(ctx, op).and_then(|callee| names.iter().position(|name| *name == callee))
                })
                .collect()
        })
        .collect();
    let loops: Vec<_> = functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
            cyclic(blocks, |block: Ptr<BasicBlock>| block.deref(ctx).succs(ctx))
                || cyclic([index], |function| call_graph[function].clone())
        })
        .collect();
    loop {
        let mut changed = false;
        for (index, function) in functions.iter().enumerate() {
            let mut summary = summaries[&names[index]].clone();
            let mut origins: HashMap<Value, BTreeSet<usize>> = function
                .get_entry_block(ctx)
                .deref(ctx)
                .arguments()
                .enumerate()
                .filter(|(_, value)| crate::types::is_reference(ctx, value.get_type(ctx)))
                .map(|(index, value)| (value, BTreeSet::from([index])))
                .collect();
            loop {
                let mut grew = false;
                for &op in &bodies[index] {
                    let inputs: Vec<_> = op.deref(ctx).operands().collect();
                    let dependencies: BTreeSet<_> = inputs
                        .iter()
                        .flat_map(|value| origins.get(value).into_iter().flatten().copied())
                        .collect();
                    for result in op.deref(ctx).results() {
                        if !crate::types::is_reference(ctx, result.get_type(ctx)) {
                            continue;
                        }
                        let set = origins.entry(result).or_default();
                        let before = set.len();
                        set.extend(dependencies.iter().copied());
                        grew |= set.len() != before;
                    }
                    let dynamic = Operation::get_op_dyn(op, ctx);
                    if let Some(branch) = op_cast::<dyn BranchOpInterface>(dynamic.as_ref()) {
                        for successor in 0..op.deref(ctx).get_num_successors() {
                            let block = op.deref(ctx).get_successor(successor);
                            for (argument, operand) in block
                                .deref(ctx)
                                .arguments()
                                .zip(branch.successor_operand_range(ctx, successor))
                            {
                                let dependency =
                                    origins.get(&inputs[operand]).cloned().unwrap_or_default();
                                let set = origins.entry(argument).or_default();
                                let before = set.len();
                                set.extend(dependency);
                                grew |= set.len() != before;
                            }
                        }
                    }
                    summary.allocates |=
                        allocates(ctx, op) || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
                    summary.may_collect |= allocates(ctx, op);
                    summary.may_suspend |= Operation::get_op::<crate::calls::AwaitOp>(op, ctx)
                        .is_some()
                        || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
                    summary.may_reenter |=
                        Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some();
                    summary.unknown_helper_bound |=
                        Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
                            || Operation::get_op::<IntrinsicOp>(op, ctx)
                                .and_then(|i| i.intrinsic(ctx))
                                .is_some_and(|i| i.may_allocate())
                            || Operation::get_op::<JsonStringifyOp>(op, ctx).is_some();
                    if Operation::get_op::<ReturnOp>(op, ctx).is_some()
                        || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx)
                            .is_some_and(|op| op.phase(ctx) == crate::foreign::Phase::Start)
                        || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                    {
                        summary.escapes.extend(dependencies.iter().copied());
                    }
                    let stored = if Operation::get_op::<crate::locals::StoreOp>(op, ctx).is_some()
                        || Operation::get_op::<FieldSetOp>(op, ctx).is_some()
                        || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
                    {
                        Some(1)
                    } else if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
                        Some(2)
                    } else {
                        None
                    };
                    if let Some(stored) = stored {
                        summary
                            .escapes
                            .extend(origins.get(&inputs[stored]).into_iter().flatten().copied());
                    }
                    if let Some(callee) = callee(ctx, op) {
                        if let Some(callee) = summaries.get(&callee) {
                            summary.allocates |= callee.allocates;
                            summary.may_collect |= callee.may_collect;
                            summary.may_suspend |= callee.may_suspend;
                            summary.may_reenter |= callee.may_reenter;
                            summary.repeated |= callee.repeated;
                            summary.unknown_helper_bound |= callee.unknown_helper_bound;
                            for &index in &callee.escapes {
                                summary.escapes.extend(
                                    origins.get(&inputs[index]).into_iter().flatten().copied(),
                                );
                            }
                        } else {
                            summary.allocates = true;
                            summary.may_collect = true;
                            summary.may_suspend = true;
                            summary.may_reenter = true;
                            summary.unknown_helper_bound = true;
                            summary.escapes.extend(dependencies);
                        }
                    }
                }
                if !grew {
                    break;
                }
            }
            summary.allocates |= is_async(ctx, function);
            summary.may_suspend |= is_async(ctx, function);
            summary.may_collect |= summary.may_suspend || summary.may_reenter;
            summary.repeated |= loops[index] && summary.allocates;
            if summaries[&names[index]] != summary {
                summaries.insert(names[index].clone(), summary);
                changed = true;
            }
        }
        if !changed {
            return summaries;
        }
    }
}
