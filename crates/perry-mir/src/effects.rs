//! Runtime capabilities and local effect constraints for ProgramFacts.
use crate::{
    calls::{AsyncCallOp, CallOp, TimerOp, is_async},
    collections::{ArrayExtendOp, ArrayNewOp, ArrayPushOp, ArraySetOp, ConcatOp},
    exceptions::{CatchOp, ErrorNewOp, RaiseOp},
    heap::{AllocOp, FieldSetOp},
    intrinsics::{
        IntrinsicOp, JsonStringifyOp, PromiseCombinatorOp, PromiseRejectedOp, PromiseResolvedOp,
    },
};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::{ControlFlowGraph, HasLabel, traversals::region::sccs_in_topological_order},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    result::Result,
    r#type::Typed,
};
use std::collections::{BTreeSet, HashMap, HashSet};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub allocates: bool,
    pub atomic_construction: bool,
    /// A safepoint is required when this module uses collection, including in callees.
    pub may_collect: bool,
    pub may_suspend: bool,
    pub may_reenter: bool,
    pub writes_record: bool,
    pub failure: Failure,
    /// Runtime-managed values or effectful services whose task independence is unproven.
    pub task_state: bool,
    /// An allocation site or allocating call can execute repeatedly within one task.
    pub repeated_allocations: bool,
    pub unknown_helper_bound: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub effects: Effects,
    /// Parameters returned, captured by an async call, or stored through an aggregate.
    pub escapes: BTreeSet<usize>,
}

impl Effects {
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

pub(crate) struct LocalEffects {
    pub effects: Effects,
    asynchronous: bool,
    recursive: bool,
    loop_calls: HashSet<Ptr<Operation>>,
}

impl LocalEffects {
    pub(crate) fn inherit(
        &self,
        mut current: Effects,
        callee: Effects,
        call: Ptr<Operation>,
        synchronous: bool,
    ) -> Effects {
        current.allocates |= callee.allocates;
        current.may_collect |=
            !current.atomic_construction && (callee.may_collect || callee.allocates);
        current.may_suspend |= callee.may_suspend;
        current.may_reenter |= callee.may_reenter;
        current.writes_record |= callee.writes_record;
        current.task_state |= callee.task_state;
        if synchronous && callee.failure == Failure::Throws {
            current.failure = if self.asynchronous {
                Failure::Rejects
            } else {
                Failure::Throws
            };
        }
        current.repeated_allocations |= callee.repeated_allocations
            || (self.recursive || self.loop_calls.contains(&call)) && callee.allocates;
        current.unknown_helper_bound |= callee.unknown_helper_bound;
        current.may_collect |= current.may_suspend || current.may_reenter;
        current
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

/// Classify each operation once; caller-dependent propagation belongs to ProgramFacts.
pub(crate) fn prepare(ctx: &Context, functions: &[FuncOp]) -> Result<Vec<LocalEffects>> {
    let names: Vec<_> = functions.iter().map(|f| f.get_symbol_name(ctx)).collect();
    let indices: HashMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(i, name)| (name, i))
        .collect();
    let mut local: Vec<_> = functions
        .iter()
        .map(|f| LocalEffects {
            effects: Effects {
                atomic_construction: crate::calls::is_atomic_construction(ctx, f),
                ..Effects::default()
            },
            asynchronous: is_async(ctx, f),
            recursive: false,
            loop_calls: HashSet::new(),
        })
        .collect();
    let mut callees = vec![Vec::new(); functions.len()];
    let mut callers = vec![Vec::new(); functions.len()];
    for (index, function) in functions.iter().enumerate() {
        let atomic = local[index].effects.atomic_construction;
        if atomic && local[index].asynchronous {
            return pliron::verify_err!(
                function.loc(ctx),
                AtomicConstructionError {
                    function: names[index].to_string(),
                    operation: "async function".into()
                }
            );
        }
        let mut effects = local[index].effects;
        let mut fails = false;
        let cyclic_blocks: HashSet<_> = sccs_in_topological_order(ctx, &function.get_region(ctx))
            .into_iter()
            .filter(|component| component.is_cyclic)
            .flat_map(|component| component.nodes)
            .collect();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                let callee = callee(ctx, op);
                let target = callee.as_ref().and_then(|name| indices.get(name)).copied();
                let allocation = allocation(ctx, op);
                if atomic {
                    let closed = callee.is_none()
                        || target.is_some_and(|i| local[i].effects.atomic_construction);
                    let unsafe_effect = allocation == Allocation::Conservative
                        || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
                        || Operation::get_op::<crate::calls::AwaitOp>(op, ctx).is_some()
                        || Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some();
                    if !closed || unsafe_effect {
                        return pliron::verify_err!(
                            Operation::get_op_dyn(op, ctx).loc(ctx),
                            AtomicConstructionError {
                                function: names[index].to_string(),
                                operation: Operation::get_op::<IntrinsicOp>(op, ctx)
                                    .and_then(|i| i.intrinsic(ctx))
                                    .map_or_else(
                                        || Operation::get_op_dyn(op, ctx).get_opid().to_string(),
                                        |i| i.name().to_owned()
                                    )
                            }
                        );
                    }
                }
                effects.writes_record |= Operation::get_op::<FieldSetOp>(op, ctx).is_some();
                effects.task_state |= op
                    .deref(ctx)
                    .operands()
                    .chain(op.deref(ctx).results())
                    .any(|value| crate::types::is_reference(ctx, value.get_type(ctx)))
                    || Operation::get_op::<IntrinsicOp>(op, ctx)
                        .is_some_and(|i| i.intrinsic(ctx).is_none_or(|i| !i.pure()));
                if Operation::get_op::<CallOp>(op, ctx).is_none() {
                    fails |= crate::intrinsics::may_throw(ctx, op);
                }
                let allocates = allocation != Allocation::None
                    || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
                effects.allocates |= allocates;
                if cyclic_blocks.contains(&block) {
                    effects.repeated_allocations |= allocates;
                    if callee.is_some() {
                        local[index].loop_calls.insert(op);
                    }
                }
                effects.may_collect |= !atomic && allocation != Allocation::None;
                effects.may_suspend |= Operation::get_op::<crate::calls::AwaitOp>(op, ctx)
                    .is_some()
                    || Operation::get_op::<AsyncCallOp>(op, ctx).is_some();
                effects.may_reenter |=
                    Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some();
                effects.unknown_helper_bound |=
                    Operation::get_op::<crate::foreign::ForeignOp>(op, ctx).is_some()
                        || Operation::get_op::<IntrinsicOp>(op, ctx)
                            .and_then(|i| i.intrinsic(ctx))
                            .is_some_and(|i| i.may_allocate())
                        || Operation::get_op::<JsonStringifyOp>(op, ctx).is_some();
                if let Some(target) = target {
                    callees[index].push(target);
                    callers[target].push(index);
                } else if callee.is_some() {
                    effects.allocates = true;
                    effects.may_collect = true;
                    effects.may_suspend = true;
                    effects.may_reenter = true;
                    effects.writes_record = true;
                    effects.task_state = true;
                    effects.unknown_helper_bound = true;
                    fails |= Operation::get_op::<CallOp>(op, ctx).is_some();
                }
            }
        }
        effects.failure = if !fails {
            Failure::None
        } else if local[index].asynchronous {
            Failure::Rejects
        } else {
            Failure::Throws
        };
        effects.allocates |= local[index].asynchronous;
        effects.may_suspend |= local[index].asynchronous;
        effects.may_collect |= effects.may_suspend || effects.may_reenter;
        local[index].effects = effects;
    }
    let graph = CallGraph {
        callees: &callees,
        callers: &callers,
    };
    for component in sccs_in_topological_order(&(), &graph) {
        if component.is_cyclic {
            for node in component.nodes {
                local[node.0].recursive = true;
                local[node.0].effects.repeated_allocations |= local[node.0].effects.allocates;
            }
        }
    }
    Ok(local)
}
