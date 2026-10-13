//! Cached interprocedural allocation, mutation, exception and reference-escape effects.
use crate::{
    calls::{AsyncCallOp, CallOp, TimerOp, is_async},
    collections::{ArrayExtendOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ConcatOp},
    control::ReturnOp,
    exceptions::{CatchOp, ErrorNewOp, RaiseOp},
    heap::{AllocOp, FieldSetOp},
    intrinsics::{
        IntrinsicOp, JsonStringifyOp, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp,
    },
};
use pliron::{
    builtin::{
        op_interfaces::{BranchOpInterface, OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::{ControlFlowGraph, HasLabel, traversals::region::sccs_in_topological_order},
    identifier::Identifier,
    op::{Op, op_cast},
    operation::Operation,
    pass::{Analysis, AnalysisManager},
    result::Result,
    r#type::Typed,
    value::Value,
};
use std::collections::{BTreeSet, HashMap, VecDeque};

#[derive(Debug, thiserror::Error)]
#[error("{function} contains {operation} outside its atomic construction group")]
struct AtomicConstructionError {
    function: String,
    operation: String,
}

/// Async failures settle a promise; they never add a synchronous status result.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Failure {
    #[default]
    None,
    Throws,
    Rejects,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub allocates: bool,
    pub atomic_construction: bool,
    /// The compiled body contains a safe point, including those in direct callees.
    pub may_collect: bool,
    pub may_suspend: bool,
    pub may_reenter: bool,
    pub writes_record: bool,
    pub failure: Failure,
    /// Runtime-managed values or effectful services whose task independence is unproven.
    pub task_state: bool,
    pub repeated: bool,
    pub unknown_helper_bound: bool,
    /// Parameters returned, captured by an async call, or stored through an aggregate.
    pub escapes: BTreeSet<usize>,
}

impl Summary {
    pub fn may_mutate_records(&self) -> bool {
        self.writes_record || self.may_suspend || self.may_reenter
    }

    pub fn requires_task(&self) -> bool {
        self.task_state
            || self.allocates
            || self.may_collect
            || self.may_suspend
            || self.may_reenter
            || self.failure != Failure::None
    }
}

/// One module snapshot, shared by MIR transformations, allocation reports and lowering.
pub struct RuntimeEffects {
    pub functions: HashMap<Identifier, Summary>,
}

impl Analysis for RuntimeEffects {
    fn name(&self) -> &str {
        "perry-runtime-effects"
    }

    fn compute(root: Ptr<Operation>, ctx: &Context, _: &mut AnalysisManager) -> Result<Self> {
        Ok(Self {
            functions: summarize(ctx, &crate::aggregates::functions(root, ctx))?,
        })
    }
}

/// Whether an operation allocates, and whether its implementation can observe caller roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocation {
    None,
    /// The helper completes without collecting, suspending, or invoking callbacks.
    /// Collection may still occur at the compiled safe point preceding the operation.
    Atomic,
    /// Caller roots must remain published throughout the operation.
    Conservative,
}

/// Allocation includes hidden temporaries and exception construction inside helpers.
pub fn allocation(ctx: &Context, op: Ptr<Operation>) -> Allocation {
    if Operation::get_op::<crate::collections::NativeJsonOp>(op, ctx).is_some() {
        return Allocation::Atomic;
    }
    if Operation::get_op::<crate::collections::ConcatSegmentsOp>(op, ctx).is_some() {
        return Allocation::Atomic;
    }
    if Operation::get_op::<crate::scalar::NumericCastOp>(op, ctx).is_some()
        && op
            .deref(ctx)
            .get_result(0)
            .get_type(ctx)
            .deref(ctx)
            .is::<crate::types::BigIntType>()
    {
        return Allocation::Atomic;
    }
    if let Some(intrinsic) = Operation::get_op::<IntrinsicOp>(op, ctx) {
        return intrinsic
            .intrinsic(ctx)
            .map_or(Allocation::Conservative, |intrinsic| intrinsic.allocation());
    }
    if Operation::get_op::<AllocOp>(op, ctx).is_some()
        || Operation::get_op::<ArrayNewOp>(op, ctx).is_some()
        || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
        || Operation::get_op::<ArrayExtendOp>(op, ctx).is_some()
        || Operation::get_op::<ArraySetOp>(op, ctx).is_some()
        || Operation::get_op::<crate::variants::VariantNewOp>(op, ctx).is_some()
        || Operation::get_op::<ConcatOp>(op, ctx).is_some()
        || Operation::get_op::<crate::collections::ConcatReuseOp>(op, ctx).is_some()
    {
        return Allocation::Atomic;
    }
    if Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
        || Operation::get_op::<TimerOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseResolvedOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseRejectedOp>(op, ctx).is_some()
        || Operation::get_op::<PromiseCombinatorOp>(op, ctx).is_some()
        || Operation::get_op::<JsonStringifyOp>(op, ctx).is_some()
        || Operation::get_op::<ErrorNewOp>(op, ctx).is_some()
        || Operation::get_op::<RaiseOp>(op, ctx).is_some()
        || Operation::get_op::<CatchOp>(op, ctx).is_some()
        || Operation::get_op::<crate::exceptions::ExceptionMessageOp>(op, ctx).is_some()
    {
        Allocation::Conservative
    } else {
        Allocation::None
    }
}

pub fn allocates(ctx: &Context, op: Ptr<Operation>) -> bool {
    allocation(ctx, op) != Allocation::None
}

fn callee(ctx: &Context, op: Ptr<Operation>) -> Option<Identifier> {
    Operation::get_op::<CallOp>(op, ctx)
        .map(|c| c.callee(ctx))
        .or_else(|| Operation::get_op::<AsyncCallOp>(op, ctx).map(|c| c.callee(ctx)))
}

struct CallGraph<'a> {
    callees: &'a [Vec<usize>],
    callers: &'a [Vec<usize>],
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FunctionIndex(usize);

impl HasLabel<()> for FunctionIndex {
    fn label(&self, _: &()) -> String {
        self.0.to_string()
    }
}

impl ControlFlowGraph<()> for CallGraph<'_> {
    type Node = FunctionIndex;
    fn num_successors(&self, _: &(), node: &FunctionIndex) -> usize {
        self.callees[node.0].len()
    }
    fn get_successor(&self, _: &(), node: &FunctionIndex, index: usize) -> FunctionIndex {
        FunctionIndex(self.callees[node.0][index])
    }
    fn num_predecessors(&self, _: &(), node: &FunctionIndex) -> usize {
        self.callers[node.0].len()
    }
    fn get_predecessor(&self, _: &(), node: &FunctionIndex, index: usize) -> FunctionIndex {
        FunctionIndex(self.callers[node.0][index])
    }
    fn entry_node(&self, _: &()) -> Option<FunctionIndex> {
        (!self.callees.is_empty()).then_some(FunctionIndex(0))
    }
    fn nodes<'a>(&'a self, _: &'a ()) -> Box<dyn Iterator<Item = FunctionIndex> + 'a> {
        Box::new((0..self.callees.len()).map(FunctionIndex))
    }
}

/// Fixed point over direct calls. Unknown bounds propagate to every caller; recursion and
/// CFG cycles never establish bounded task allocation. Escape provenance follows SSA joins,
/// aggregates and calls. Scalar field reads do not retain their containing object.
fn summarize(ctx: &Context, functions: &[FuncOp]) -> Result<HashMap<Identifier, Summary>> {
    let names: Vec<_> = functions.iter().map(|f| f.get_symbol_name(ctx)).collect();
    let mut summaries: HashMap<_, _> = names
        .iter()
        .cloned()
        .map(|name| (name, Summary::default()))
        .collect();
    let bodies: Vec<_> = functions
        .iter()
        .map(|function| crate::aggregates::operations(ctx, function))
        .collect();
    let index_of: HashMap<&Identifier, usize> = names
        .iter()
        .enumerate()
        .map(|(index, name)| (name, index))
        .collect();
    let atomic: Vec<_> = functions
        .iter()
        .map(|f| crate::calls::is_atomic_construction(ctx, f))
        .collect();
    for (index, body) in bodies.iter().enumerate().filter(|(i, _)| atomic[*i]) {
        if is_async(ctx, &functions[index]) {
            return pliron::verify_err!(
                functions[index].loc(ctx),
                AtomicConstructionError {
                    function: names[index].to_string(),
                    operation: "async function".into()
                }
            );
        }
        for &op in body {
            let closed_call =
                callee(ctx, op).is_none_or(|name| index_of.get(&name).is_some_and(|&i| atomic[i]));
            let unsafe_effect = allocation(ctx, op) == Allocation::Conservative
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                || Operation::get_op::<crate::calls::AwaitOp>(op, ctx).is_some()
                || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some();
            if !closed_call || unsafe_effect {
                return pliron::verify_err!(
                    Operation::get_op_dyn(op, ctx).loc(ctx),
                    AtomicConstructionError {
                        function: names[index].to_string(),
                        operation: Operation::get_op::<IntrinsicOp>(op, ctx)
                            .and_then(|intrinsic| intrinsic.intrinsic(ctx))
                            .map_or_else(
                                || Operation::get_op_dyn(op, ctx).get_opid().to_string(),
                                |intrinsic| intrinsic.name().to_owned()
                            )
                    }
                );
            }
        }
    }
    let call_graph: Vec<Vec<usize>> = bodies
        .iter()
        .map(|body| {
            body.iter()
                .filter_map(|&op| callee(ctx, op).and_then(|callee| index_of.get(&callee).copied()))
                .collect()
        })
        .collect();
    let mut callers = vec![Vec::new(); functions.len()];
    for (caller, callees) in call_graph.iter().enumerate() {
        for &callee in callees {
            callers[callee].push(caller);
        }
    }
    let graph = CallGraph {
        callees: &call_graph,
        callers: &callers,
    };
    let mut recursive = vec![false; functions.len()];
    for component in sccs_in_topological_order(&(), &graph).into_iter().rev() {
        let reaches_cycle = component.is_cyclic
            || component
                .nodes
                .iter()
                .any(|node| call_graph[node.0].iter().any(|&callee| recursive[callee]));
        for node in component.nodes {
            recursive[node.0] = reaches_cycle;
        }
    }
    let loops: Vec<_> = functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            recursive[index]
                || sccs_in_topological_order(ctx, &function.get_region(ctx))
                    .iter()
                    .any(|component| component.is_cyclic)
        })
        .collect();
    let origins: Vec<_> = functions
        .iter()
        .map(|function| {
            let mut origins: HashMap<Value, BTreeSet<usize>> = HashMap::new();
            let mut pending: Vec<_> = function
                .get_entry_block(ctx)
                .deref(ctx)
                .arguments()
                .enumerate()
                .filter(|(_, value)| crate::types::is_reference(ctx, value.get_type(ctx)))
                .map(|(index, value)| (value, index))
                .collect();
            while let Some((value, origin)) = pending.pop() {
                if !origins.entry(value).or_default().insert(origin) {
                    continue;
                }
                for usage in value.uses(ctx) {
                    let op = usage.user_op();
                    pending.extend(
                        op.deref(ctx)
                            .results()
                            .filter(|result| crate::types::is_reference(ctx, result.get_type(ctx)))
                            .map(|result| (result, origin)),
                    );
                    let dynamic = Operation::get_op_dyn(op, ctx);
                    if let Some(branch) = op_cast::<dyn BranchOpInterface>(dynamic.as_ref()) {
                        let operand = usage.find_index(ctx);
                        for successor in 0..op.deref(ctx).get_num_successors() {
                            let range = branch.successor_operand_range(ctx, successor);
                            if range.contains(&operand) {
                                let block = op.deref(ctx).get_successor(successor);
                                pending.push((
                                    block.deref(ctx).get_argument(operand - range.start),
                                    origin,
                                ));
                            }
                        }
                    }
                }
            }
            origins
        })
        .collect();
    let mut pending: VecDeque<_> = (0..functions.len()).collect();
    let mut queued = vec![true; functions.len()];
    while let Some(index) = pending.pop_front() {
        queued[index] = false;
        let function = &functions[index];
        let origins = &origins[index];
        let mut summary = summaries[&names[index]].clone();
        let mut fails = summary.failure != Failure::None;
        for &op in &bodies[index] {
            let operation = op.deref(ctx);
            let dependencies = || {
                operation
                    .operands()
                    .flat_map(|value| origins.get(&value).into_iter().flatten().copied())
            };
            summary.writes_record |= Operation::get_op::<FieldSetOp>(op, ctx).is_some();
            summary.task_state |= op
                .deref(ctx)
                .operands()
                .chain(op.deref(ctx).results())
                .any(|value| crate::types::is_reference(ctx, value.get_type(ctx)))
                || Operation::get_op::<IntrinsicOp>(op, ctx)
                    .is_some_and(|op| op.intrinsic(ctx).is_none_or(|intrinsic| !intrinsic.pure()));
            if Operation::get_op::<CallOp>(op, ctx).is_none() {
                fails |= crate::intrinsics::may_throw(ctx, op);
            }
            summary.allocates |=
                allocates(ctx, op) || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
            summary.may_collect |= !atomic[index] && allocates(ctx, op);
            summary.may_suspend |= Operation::get_op::<crate::calls::AwaitOp>(op, ctx).is_some()
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
            summary.may_reenter |=
                Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some();
            summary.unknown_helper_bound |= Operation::get_op::<crate::foreign::ForeignOp>(op, ctx)
                .is_some()
                || Operation::get_op::<IntrinsicOp>(op, ctx)
                    .and_then(|i| i.intrinsic(ctx))
                    .is_some_and(|i| i.may_allocate())
                || Operation::get_op::<JsonStringifyOp>(op, ctx).is_some();
            if Operation::get_op::<ReturnOp>(op, ctx).is_some()
                || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx)
                    .is_some_and(|op| op.phase(ctx) == crate::foreign::Phase::Start)
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
            {
                summary.escapes.extend(dependencies());
            }
            let stored = if Operation::get_op::<crate::locals::StoreOp>(op, ctx).is_some()
                || Operation::get_op::<FieldSetOp>(op, ctx).is_some()
                || Operation::get_op::<ArrayPushOp>(op, ctx).is_some()
                || Operation::get_op::<ArrayExtendOp>(op, ctx).is_some()
            {
                Some(1)
            } else if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
                Some(2)
            } else {
                None
            };
            if let Some(stored) = stored {
                summary.escapes.extend(
                    origins
                        .get(&operation.get_operand(stored))
                        .into_iter()
                        .flatten()
                        .copied(),
                );
            }
            if let Some(callee) = callee(ctx, op) {
                if let Some(callee) = summaries.get(&callee) {
                    summary.allocates |= callee.allocates;
                    summary.may_collect |=
                        !atomic[index] && (callee.may_collect || callee.allocates);
                    summary.may_suspend |= callee.may_suspend;
                    summary.may_reenter |= callee.may_reenter;
                    summary.writes_record |= callee.writes_record;
                    summary.task_state |= callee.task_state;
                    fails |= Operation::get_op::<CallOp>(op, ctx).is_some()
                        && callee.failure == Failure::Throws;
                    summary.repeated |= callee.repeated;
                    summary.unknown_helper_bound |= callee.unknown_helper_bound;
                    for &index in &callee.escapes {
                        summary.escapes.extend(
                            origins
                                .get(&operation.get_operand(index))
                                .into_iter()
                                .flatten()
                                .copied(),
                        );
                    }
                } else {
                    summary.allocates = true;
                    summary.may_collect = true;
                    summary.may_suspend = true;
                    summary.may_reenter = true;
                    summary.writes_record = true;
                    summary.task_state = true;
                    fails |= Operation::get_op::<CallOp>(op, ctx).is_some();
                    summary.unknown_helper_bound = true;
                    summary.escapes.extend(dependencies());
                }
            }
        }
        summary.atomic_construction = atomic[index];
        summary.failure = if !fails {
            Failure::None
        } else if is_async(ctx, function) {
            Failure::Rejects
        } else {
            Failure::Throws
        };
        summary.allocates |= is_async(ctx, function);
        summary.may_suspend |= is_async(ctx, function);
        summary.may_collect |= summary.may_suspend || summary.may_reenter;
        summary.repeated |= loops[index] && summary.allocates;
        if summaries[&names[index]] != summary {
            summaries.insert(names[index].clone(), summary);
            for &caller in &callers[index] {
                if !queued[caller] {
                    pending.push_back(caller);
                    queued[caller] = true;
                }
            }
        }
    }
    Ok(summaries)
}
