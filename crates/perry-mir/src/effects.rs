//! Runtime capabilities and local effect constraints for ProgramFacts.
use crate::{
    calls::{callee, is_async},
    collections::StringConstOp,
    semantics::{OperationKind, PrimitiveSemantics},
};
use pliron::{
    builtin::{
        op_interfaces::{OneRegionInterface, SymbolOpInterface},
        ops::FuncOp,
    },
    context::{Context, Ptr},
    graph::traversals::region::sccs_in_topological_order,
    linked_list::ContainsLinkedList,
    op::{Op, op_cast},
    operation::Operation,
    result::Result,
    r#type::Typed,
};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug)]
struct AtomicConstructionError {
    function: String,
    operation: String,
}

impl core::fmt::Display for AtomicConstructionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} contains {} outside its atomic construction group",
            self.function, self.operation
        )
    }
}

impl core::error::Error for AtomicConstructionError {}

/// Failure outcome of a function: synchronous status return (`Throws`) or async promise
/// rejection (`Rejects`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Failure {
    #[default]
    None,
    Throws {
        escapes: bool,
    },
    Rejects {
        escapes: bool,
    },
}

impl Failure {
    pub fn throws(self) -> bool {
        matches!(self, Self::Throws { .. })
    }

    pub fn escapes(self) -> bool {
        matches!(
            self,
            Self::Throws { escapes: true } | Self::Rejects { escapes: true }
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub allocates: bool,
    pub atomic_construction: bool,
    /// A safepoint is required when this module uses collection, including in callees.
    pub may_collect: bool,
    pub may_suspend: bool,
    pub may_reenter: bool,
    /// A host operation can remain pending when the source function returns.
    pub host_work: bool,
    /// An unhandled await or synchronous callee exposes the async boundary's error channel.
    pub unhandled_await: bool,
    pub writes_record: bool,
    pub writes_state: bool,
    pub(crate) shrinks_arrays: bool,
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
    pub(crate) fn may_mutate_records(&self) -> bool {
        self.writes_record || self.may_suspend || self.may_reenter
    }

    pub(crate) fn may_mutate_state(&self) -> bool {
        self.writes_state || self.may_mutate_records()
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
    pub(crate) effects: Effects,
    asynchronous: bool,
    recursive: bool,
    loop_calls: HashSet<Ptr<Operation>>,
    handled: HashSet<Ptr<Operation>>,
}

impl LocalEffects {
    pub(crate) fn fail(&self, current: Failure, op: Ptr<Operation>, escapes: bool) -> Failure {
        let escapes = current.escapes() || escapes && !self.handled.contains(&op);
        if self.asynchronous {
            Failure::Rejects { escapes }
        } else {
            Failure::Throws { escapes }
        }
    }

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
        current.host_work |= callee.host_work;
        current.unhandled_await |=
            synchronous && callee.unhandled_await && !self.handled.contains(&call);
        current.writes_record |= callee.writes_record;
        current.writes_state |= callee.writes_state;
        current.shrinks_arrays |= callee.shrinks_arrays;
        current.task_state |= callee.task_state;
        if synchronous && callee.failure.throws() {
            current.failure = self.fail(current.failure, call, callee.failure.escapes());
        }
        current.repeated_allocations |= callee.repeated_allocations
            || (self.recursive || self.loop_calls.contains(&call)) && callee.allocates;
        current.unknown_helper_bound |= callee.unknown_helper_bound;
        current.may_collect |= current.may_suspend || current.may_reenter;
        current
    }
}

/// Explicit storage mutations. Operand zero is the destination.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mutation {
    Field { slot: Option<usize> },
    Element { index: Option<usize> },
    Extend,
    Truncate,
    Local,
}

impl Mutation {
    pub(crate) fn stored_operand(self) -> Option<usize> {
        match self {
            Self::Element { index: Some(_) } => Some(2),
            Self::Truncate => None,
            _ => Some(1),
        }
    }
}

/// Whether an operation allocates, and whether its implementation can observe caller roots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Allocation {
    None,
    /// Single-step allocation where collection may occur at the compiled safe point preceding
    /// the operation.
    Atomic,
    /// Caller roots must remain published throughout the operation.
    Conservative,
}

/// Allocation includes hidden temporaries and exception construction inside helpers.
pub fn allocation(ctx: &Context, op: Ptr<Operation>) -> Allocation {
    op_cast::<dyn PrimitiveSemantics>(Operation::get_op_dyn(op, ctx).as_ref())
        .map_or(Allocation::None, |semantics| semantics.allocation(ctx))
}

pub fn allocates(ctx: &Context, op: Ptr<Operation>) -> bool {
    allocation(ctx, op) != Allocation::None
}

/// Classify each operation once. Caller-dependent propagation runs in `ProgramFacts`.
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
            handled: HashSet::new(),
        })
        .collect();
    let mut callees = vec![Vec::new(); functions.len()];
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
        let cyclic_blocks: HashSet<_> = sccs_in_topological_order(ctx, &function.get_region(ctx))
            .into_iter()
            .filter(|component| component.is_cyclic)
            .flat_map(|component| component.nodes)
            .collect();
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                let callee = callee(ctx, op);
                let target = callee.as_ref().and_then(|name| indices.get(name)).copied();
                if crate::exceptions::is_handled(ctx, op) {
                    local[index].handled.insert(op);
                }
                let kind = OperationKind::classify(ctx, op);
                let allocation = allocation(ctx, op);
                let intrinsic = match kind {
                    OperationKind::Intrinsic(i) => Some(i),
                    _ => None,
                };
                let call = kind
                    == OperationKind::Call {
                        asynchronous: false,
                    };
                let async_call = kind == OperationKind::Call { asynchronous: true };
                let wait = kind == OperationKind::Await;
                let foreign = matches!(kind, OperationKind::Foreign(_));
                if atomic {
                    let closed = callee.is_none()
                        || target.is_some_and(|i| local[i].effects.atomic_construction);
                    let unsafe_effect =
                        allocation == Allocation::Conservative || async_call || wait || foreign;
                    if !closed || unsafe_effect {
                        let operation = intrinsic.map_or_else(
                            || Operation::get_opid(op, ctx).to_string(),
                            |i| i.name().to_owned(),
                        );
                        return pliron::verify_err!(
                            Operation::get_op_dyn(op, ctx).loc(ctx),
                            AtomicConstructionError {
                                function: names[index].to_string(),
                                operation
                            }
                        );
                    }
                }
                let writes_record = matches!(kind, OperationKind::Mutate(Mutation::Field { .. }));
                effects.writes_record |= writes_record;
                effects.writes_state |= matches!(kind, OperationKind::Mutate(_)) || foreign;
                effects.shrinks_arrays |= kind == OperationKind::Mutate(Mutation::Truncate);
                let operation = op.deref(ctx);
                effects.task_state |= intrinsic
                    .is_some_and(|i| i.policy().effect != crate::intrinsics::ServiceEffect::Pure)
                    || operation
                        .operands()
                        .chain(operation.results())
                        .any(|value| {
                            crate::types::is_reference(ctx, value.get_type(ctx))
                                && value
                                    .defining_op()
                                    .is_none_or(|def| !Operation::is_op::<StringConstOp>(def, ctx))
                        });
                if !call && !wait && crate::intrinsics::may_throw(ctx, op) {
                    effects.failure = local[index].fail(effects.failure, op, true);
                }
                effects.unhandled_await |= wait && !local[index].handled.contains(&op);
                let allocates = allocation != Allocation::None || async_call;
                effects.allocates |= allocates;
                if cyclic_blocks.contains(&block) {
                    effects.repeated_allocations |= allocates;
                    if callee.is_some() {
                        local[index].loop_calls.insert(op);
                    }
                }
                effects.may_collect |= !atomic && allocation != Allocation::None;
                effects.may_suspend |= wait || async_call;
                effects.may_reenter |= foreign;
                effects.host_work |= foreign
                    || kind == OperationKind::Timer
                    || intrinsic.is_some()
                        && operation.results().any(|value| {
                            value
                                .get_type(ctx)
                                .deref(ctx)
                                .is::<crate::types::PromiseType>()
                        });
                effects.unknown_helper_bound |= foreign
                    || intrinsic.is_some_and(|i| i.policy().allocation == Allocation::Conservative)
                    || matches!(kind, OperationKind::Stringify | OperationKind::JsonDecode);
                if let Some(target) = target {
                    callees[index].push(target);
                } else if callee.is_some() {
                    // An unknown callee may do anything but break atomic construction.
                    effects = Effects {
                        allocates: true,
                        may_collect: true,
                        may_suspend: true,
                        may_reenter: true,
                        host_work: true,
                        writes_record: true,
                        writes_state: true,
                        shrinks_arrays: true,
                        task_state: true,
                        unknown_helper_bound: true,
                        ..effects
                    };
                    if call {
                        effects.failure = local[index].fail(effects.failure, op, true);
                    }
                }
            }
        }
        effects.allocates |= local[index].asynchronous;
        effects.may_suspend |= local[index].asynchronous;
        effects.may_collect |= effects.may_suspend || effects.may_reenter;
        local[index].effects = effects;
    }
    // A function on a call cycle reaches itself; each recursive invocation allocates again.
    for (index, local) in local.iter_mut().enumerate() {
        let callees_of = |function: usize| callees[function].iter().copied();
        if crate::reachable(callees_of(index), callees_of).contains(&index) {
            local.recursive = true;
            local.effects.repeated_allocations |= local.effects.allocates;
        }
    }
    Ok(local)
}
