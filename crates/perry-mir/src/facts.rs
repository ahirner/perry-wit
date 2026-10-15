//! Cached numeric, runtime-effect and parameter-provenance constraints over SSA values.
//!
//! Contexts contain domains, never constants or intervals. A single worklist propagates
//! representation facts forward and required widths backward, including recursive calls.
//! Rewriting runs only after the analysis converges, fixing semantic wrap points before
//! SCCP, inlining and other algebraic transformations.
use crate::{
    calls::{AsyncCallOp, AwaitOp, CallOp, TimerOp},
    control::ReturnOp,
    intrinsics::PromiseResolvedOp,
    scalar::NumericCastOp,
    types::PromiseType,
};
use pliron::{
    builtin::{
        attributes::{IdentifierAttr, TypeAttr},
        op_interfaces::{BranchOpInterface, SymbolOpInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::FunctionType,
    },
    context::{Context, Ptr},
    identifier::Identifier,
    op::{Op, op_cast},
    operation::Operation,
    pass::{Analysis, AnalysisManager},
    result::Result,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

mod domains;
mod heap;
mod rewrite;
mod transfer;
use domains::{
    Fact, Integer, Interval, Range, ResultDomain, arithmetic_range, comparison_domain,
    constant_range, literal,
};
pub(crate) use rewrite::Specialize;
mod outcomes;
use outcomes::{Outcomes, promise_payload};

pliron::dict_key!(WIT_SIGNATURE, "perry_wit_signature");
pliron::dict_key!(SPECIALIZATION, "perry_specialization");

/// Attach validated WIT numeric constraints before optimizing a source function.
pub fn constrain(
    ctx: &mut Context,
    function: &FuncOp,
    args: Vec<TypeHandle>,
    results: Vec<TypeHandle>,
) {
    let signature = FunctionType::get(ctx, args, results);
    function
        .get_operation()
        .deref_mut(ctx)
        .attributes
        .set(WIT_SIGNATURE.clone(), TypeAttr::new(signature.into()));
}

/// Return the attached WIT signature constraint, if any.
pub fn wit_signature(ctx: &Context, function: &FuncOp) -> Option<TypeHandle> {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<TypeAttr>(&WIT_SIGNATURE)
        .map(|attr| attr.get_type(ctx))
}

/// Implementation selected for a constrained boundary, or the original function.
pub fn implementation(ctx: &Context, function: &FuncOp) -> Identifier {
    function
        .get_operation()
        .deref(ctx)
        .attributes
        .get::<IdentifierAttr>(&SPECIALIZATION)
        .map(|name| name.clone().into())
        .unwrap_or_else(|| function.get_symbol_name(ctx))
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    function: Identifier,
    args: Vec<Option<Integer>>,
    results: Vec<ResultDomain>,
    /// Actual MIR ranges never assume a specialization that has not been rewritten yet.
    specialize: bool,
}

#[derive(Clone, Copy)]
enum Constraint {
    Op {
        op: Ptr<Operation>,
        heap: heap::OperationKind,
    },
    Edge {
        from: Value,
        to: Value,
    },
}

struct Body {
    function: FuncOp,
    constraints: Vec<Constraint>,
    dependents: HashMap<Value, Vec<usize>>,
    parameters: Vec<Value>,
    declared_args: Vec<Option<Integer>>,
    declared_results: Vec<Option<Integer>>,
    effects: crate::effects::LocalEffects,
    external: bool,
}

impl Body {
    fn new(ctx: &Context, function: FuncOp, effects: crate::effects::LocalEffects) -> Self {
        let mut constraints = Vec::new();
        let mut dependents: HashMap<Value, Vec<usize>> = HashMap::new();
        for op in crate::aggregates::operations(ctx, &function) {
            let index = constraints.len();
            constraints.push(Constraint::Op {
                op,
                heap: heap::OperationKind::classify(ctx, op),
            });
            for value in op.deref(ctx).operands().chain(op.deref(ctx).results()) {
                dependents.entry(value).or_default().push(index);
            }
            let dynamic = Operation::get_op_dyn(op, ctx);
            if let Some(branch) = op_cast::<dyn BranchOpInterface>(&*dynamic) {
                for successor in 0..op.deref(ctx).get_num_successors() {
                    let block = op.deref(ctx).get_successor(successor);
                    for (to, from) in block
                        .deref(ctx)
                        .arguments()
                        .zip(branch.successor_operands(ctx, successor))
                    {
                        let index = constraints.len();
                        constraints.push(Constraint::Edge { from, to });
                        dependents.entry(from).or_default().push(index);
                        dependents.entry(to).or_default().push(index);
                    }
                }
            }
        }
        let signature = function
            .get_operation()
            .deref(ctx)
            .attributes
            .get::<TypeAttr>(&WIT_SIGNATURE)
            .and_then(|attr| {
                TypedHandle::<FunctionType>::from_handle(attr.get_type(ctx), ctx).ok()
            });
        let fn_ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx).ok();
        let declared_args = match signature {
            Some(sig) => sig
                .deref(ctx)
                .arg_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect(),
            None => vec![None; fn_ty.map_or(0, |ty| ty.deref(ctx).arg_types().len())],
        };
        let declared_results = match signature {
            Some(sig) => sig
                .deref(ctx)
                .res_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect(),
            None => vec![None; fn_ty.map_or(0, |ty| ty.deref(ctx).res_types().len())],
        };
        Self {
            external: crate::calls::is_exported(ctx, &function)
                || function
                    .get_operation()
                    .deref(ctx)
                    .attributes
                    .0
                    .contains_key(&WIT_SIGNATURE),
            function,
            effects,
            constraints,
            dependents,
            parameters: function
                .get_entry_block(ctx)
                .deref(ctx)
                .arguments()
                .collect(),
            declared_args,
            declared_results,
        }
    }
}

const MAX_PRECISE_CONTEXTS: usize = 32;

#[derive(Default)]
struct State {
    facts: HashMap<Value, Fact>,
    ranges: HashMap<Value, Range>,
    range_expansions: HashMap<Value, u8>,
    required: HashMap<Value, Integer>,
    original_payloads: HashSet<Value>,
    calls: HashMap<Ptr<Operation>, usize>,
    results: Vec<ResultFact>,
    callers: BTreeSet<(usize, usize)>,
    origins: HashMap<Value, BTreeSet<usize>>,
    summary: crate::effects::Summary,
    heap_values: HashMap<Value, usize>,
    outcomes: HashMap<Value, Outcomes>,
}

#[derive(Clone, Copy, Default)]
struct ResultFact {
    domain: Fact,
    range: Range,
    expansions: u8,
    heap: Option<usize>,
    outcome: Outcomes,
}

impl State {
    fn fact(&self, value: Value) -> Fact {
        self.facts.get(&value).copied().unwrap_or_default()
    }
    fn range(&self, value: Value) -> Range {
        self.ranges.get(&value).copied().unwrap_or_default()
    }
    fn representation(&self, value: Value) -> Option<Integer> {
        match self.fact(value) {
            Fact::Integer(integer) => Some(integer),
            Fact::Literal => self.required.get(&value).copied(),
            _ => None,
        }
    }
}

/// Cached result for one unchanged module. Domain propagation is Perry semantics;
/// scheduling, cloning, verification and invalidation use pliron infrastructure.
pub struct ProgramFacts {
    bodies: HashMap<Identifier, Body>,
    keys: Vec<Key>,
    states: Vec<State>,
    roots: Vec<(FuncOp, usize)>,
    generic: HashMap<Identifier, usize>,
    pub constraint_visits: usize,
    /// Alias sets collapsed to a typed summary after exceeding 32 allocation sites.
    pub summarized_heap_sets: usize,
    /// Specialized call contexts collapsed to a summary context after exceeding 32 precise contexts per function.
    pub summarized_contexts: usize,
    /// Value or result intervals widened to domain bounds after expanding more than once.
    pub range_widenings: usize,
    heap: heap::HeapFacts,
}

impl ProgramFacts {
    pub fn effects(&self, function: &Identifier) -> Option<&crate::effects::Summary> {
        self.generic
            .get(function)
            .map(|&id| &self.states[id].summary)
    }

    /// Non-negative integral values, including loop-carried counts after range widening.
    pub(crate) fn is_count(&self, ctx: &Context, function: &FuncOp, value: Value) -> bool {
        let state = &self.states[self.generic[&function.get_symbol_name(ctx)]];
        crate::optional::optional_value(ctx, value.get_type(ctx)).is_none()
            && matches!(
                state.range(value),
                Range::Bounds(Interval { lower: 0.., .. })
            )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Operation(usize, usize),
    OpenObject(usize),
}

#[derive(Default)]
struct Worklist {
    pending: VecDeque<Work>,
    operations: Vec<Vec<bool>>,
    objects: Vec<bool>,
}

impl Worklist {
    fn enqueue(&mut self, item: Work) {
        let queued = match item {
            Work::Operation(context, index) => &mut self.operations[context][index],
            Work::OpenObject(object) => &mut self.objects[object],
        };
        if !*queued {
            *queued = true;
            self.pending.push_back(item);
        }
    }

    fn pop(&mut self) -> Option<Work> {
        let item = self.pending.pop_front()?;
        match item {
            Work::Operation(context, index) => self.operations[context][index] = false,
            Work::OpenObject(object) => self.objects[object] = false,
        }
        Some(item)
    }
}

struct Solver {
    analysis: ProgramFacts,
    contexts: HashMap<Key, usize>,
    precise_counts: HashMap<Identifier, usize>,
    work: Worklist,
    width_defaults: Vec<(usize, usize, Value)>,
}

impl Solver {
    fn context(&mut self, ctx: &Context, key: Key) -> usize {
        if let Some(&id) = self.contexts.get(&key) {
            return id;
        }
        if key.specialize {
            let count = self.precise_counts.entry(key.function.clone()).or_insert(0);
            if *count >= MAX_PRECISE_CONTEXTS {
                self.analysis.summarized_contexts += 1;
            }
            *count += 1;
        }
        let id = self.allocate_context(ctx, key.clone());
        self.contexts.insert(key, id);
        id
    }

    fn allocate_context(&mut self, ctx: &Context, key: Key) -> usize {
        let id = self.analysis.keys.len();
        let body = &self.analysis.bodies[&key.function];
        let mut state = State {
            results: vec![ResultFact::default(); key.results.len()],
            summary: crate::effects::Summary {
                effects: body.effects.effects,
                ..Default::default()
            },
            ..State::default()
        };
        for (position, (&parameter, domain)) in body.parameters.iter().zip(&key.args).enumerate() {
            if body.external {
                state
                    .outcomes
                    .insert(parameter, Outcomes::unknown(ctx, parameter.get_type(ctx)));
            }
            if crate::types::is_reference(ctx, parameter.get_type(ctx)) {
                state.origins.insert(parameter, BTreeSet::from([position]));
            }
            state
                .facts
                .insert(parameter, domain.map_or(Fact::Original, Fact::Integer));
            state.ranges.insert(
                parameter,
                domain.map_or(
                    if body.external {
                        Range::Unknown
                    } else {
                        Range::Bottom
                    },
                    |domain| Range::Bounds(domain.bounds()),
                ),
            );
        }
        let constraints = body.constraints.len();
        self.analysis.keys.push(key);
        self.analysis.states.push(state);
        self.work.operations.push(vec![false; constraints]);
        for constraint in 0..constraints {
            self.work.enqueue(Work::Operation(id, constraint));
        }
        self.seed_heap_parameters(ctx, id);
        // The signature is fixed for this context, even when requirements propagate
        // through the parameter: conversions belong at its uses, not at its entry.
        id
    }

    fn changed(&mut self, id: usize, value: Value) {
        let body = &self.analysis.bodies[&self.analysis.keys[id].function];
        let dependents = body.dependents.get(&value).into_iter().flatten();
        let parameter = body.parameters.contains(&value);
        for &constraint in dependents {
            self.work.enqueue(Work::Operation(id, constraint));
        }
        if parameter {
            self.notify_callers(id);
        }
    }

    fn notify_callers(&mut self, id: usize) {
        for &caller in &self.analysis.states[id].callers {
            self.work.enqueue(Work::Operation(caller.0, caller.1));
        }
    }

    fn propagate_origins(&mut self, id: usize, from: Value, to: Value) {
        let origins = &mut self.analysis.states[id].origins;
        if from == to || !origins.contains_key(&from) {
            return;
        }
        origins.entry(to).or_default();
        let [Some(from), Some(target)] = origins.get_disjoint_mut([&from, &to]) else {
            return;
        };
        let previous = target.len();
        target.extend(from.iter().copied());
        if previous != target.len() {
            self.changed(id, to);
        }
    }

    fn escape(&mut self, id: usize, value: Value) {
        let state = &mut self.analysis.states[id];
        let previous = state.summary.escapes.len();
        state
            .summary
            .escapes
            .extend(state.origins.get(&value).into_iter().flatten().copied());
        if previous != state.summary.escapes.len() {
            self.notify_callers(id);
        }
    }
}

impl Analysis for ProgramFacts {
    fn name(&self) -> &str {
        "perry-program-facts"
    }

    fn compute(root: Ptr<Operation>, ctx: &Context, _: &mut AnalysisManager) -> Result<Self> {
        let functions = crate::aggregates::functions(root, ctx);
        let effects = crate::effects::prepare(ctx, &functions)?;
        let mut solver = Solver {
            analysis: Self {
                bodies: functions
                    .iter()
                    .zip(effects)
                    .map(|(&function, effects)| {
                        (
                            function.get_symbol_name(ctx),
                            Body::new(ctx, function, effects),
                        )
                    })
                    .collect(),
                keys: Vec::new(),
                states: Vec::new(),
                roots: Vec::new(),
                generic: HashMap::new(),
                constraint_visits: 0,
                summarized_heap_sets: 0,
                summarized_contexts: 0,
                range_widenings: 0,
                heap: heap::HeapFacts::default(),
            },
            contexts: HashMap::new(),
            precise_counts: HashMap::new(),
            work: Worklist::default(),
            width_defaults: Vec::new(),
        };
        let has_integer_constraints = functions.iter().any(|function| {
            (!crate::calls::is_exported(ctx, function)
                && function
                    .get_operation()
                    .deref(ctx)
                    .attributes
                    .0
                    .contains_key(&WIT_SIGNATURE))
                || crate::aggregates::operations(ctx, function)
                    .into_iter()
                    .any(|op| {
                        Operation::get_op::<NumericCastOp>(op, ctx).is_some()
                            && Integer::from_type(ctx, op.deref(ctx).get_result(0).get_type(ctx))
                                .is_some()
                    })
        });
        for function in functions {
            let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?;
            let args = ty
                .deref(ctx)
                .arg_types()
                .iter()
                .map(|&ty| Integer::from_type(ctx, ty))
                .collect();
            let generic = solver.context(
                ctx,
                Key {
                    function: function.get_symbol_name(ctx),
                    args,
                    results: ty
                        .deref(ctx)
                        .res_types()
                        .iter()
                        .map(|&ty| Integer::from_type(ctx, ty).into())
                        .collect(),
                    specialize: false,
                },
            );
            solver
                .analysis
                .generic
                .insert(function.get_symbol_name(ctx), generic);
            if function
                .get_operation()
                .deref(ctx)
                .attributes
                .0
                .contains_key(&SPECIALIZATION)
            {
                continue;
            }
            let signature = function
                .get_operation()
                .deref(ctx)
                .attributes
                .get::<TypeAttr>(&WIT_SIGNATURE)
                .map(|attr| attr.get_type(ctx));
            let is_root = signature.is_some()
                || (has_integer_constraints
                    && (crate::calls::is_exported(ctx, &function)
                        || function.get_symbol_name(ctx).as_ref() == crate::globals::INITIALIZER
                        || function.get_symbol_name(ctx).as_ref() == "perry_main"));
            if !is_root {
                continue;
            }
            let (args, results) = if let Some(signature) = signature {
                let signature = TypedHandle::<FunctionType>::from_handle(signature, ctx)?;
                (
                    signature
                        .deref(ctx)
                        .arg_types()
                        .iter()
                        .map(|&ty| Integer::from_type(ctx, ty))
                        .collect(),
                    signature
                        .deref(ctx)
                        .res_types()
                        .iter()
                        .map(|&ty| Integer::from_type(ctx, ty).into())
                        .collect(),
                )
            } else {
                (
                    ty.deref(ctx)
                        .arg_types()
                        .iter()
                        .map(|&ty| Integer::from_type(ctx, ty))
                        .collect(),
                    ty.deref(ctx)
                        .res_types()
                        .iter()
                        .map(|&ty| {
                            Integer::from_type(ctx, ty)
                                .map_or(ResultDomain::Original, ResultDomain::Integer)
                        })
                        .collect(),
                )
            };
            let id = solver.context(
                ctx,
                Key {
                    function: function.get_symbol_name(ctx),
                    args,
                    results,
                    specialize: true,
                },
            );
            solver.analysis.roots.push((function, id));
        }
        loop {
            while let Some(item) = solver.work.pop() {
                solver.analysis.constraint_visits += 1;
                match item {
                    Work::Operation(id, index) => solver.process(ctx, id, index),
                    Work::OpenObject(object) => solver.open_object(ctx, object),
                }
            }
            // Every default is applied after propagation settles, before the next queue drain.
            solver
                .width_defaults
                .sort_unstable_by_key(|&(id, index, _)| std::cmp::Reverse((id, index)));
            let mut defaulted = false;
            while let Some((id, _, value)) = solver.width_defaults.pop() {
                solver.analysis.constraint_visits += 1;
                if !solver.analysis.states[id].required.contains_key(&value)
                    && let Some(integer) = solver.analysis.states[id].fact(value).integer()
                {
                    solver.require(id, value, Some(integer.unconstrained()));
                    defaulted = true;
                }
            }
            if !defaulted {
                break;
            }
        }
        Ok(solver.analysis)
    }
}
