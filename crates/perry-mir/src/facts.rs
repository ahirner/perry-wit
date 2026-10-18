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
use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
};

mod domains;
mod heap;
mod rewrite;
mod transfer;
use domains::{
    Fact, Integer, Interval, Range, ResultDomain, arithmetic_range, comparison_domain,
    constant_range, integer_arithmetic, literal,
};
pub(crate) use rewrite::Specialize;
pub use rewrite::{verify_lossless_module, verify_lossless_operations};
mod outcomes;
use outcomes::{Outcomes, promise_payload};

pliron::dict_key!(WIT_SIGNATURE, "perry_wit_signature");
pliron::dict_key!(SPECIALIZATION, "perry_specialization");

/// Attach validated WIT numeric constraints before optimizing a source function.
pub fn constrain_signature(
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

/// The attached WIT signature as the function type `set_wit_signature` stores.
fn wit_function_type(
    ctx: &Context,
    function: &FuncOp,
) -> Result<Option<TypedHandle<FunctionType>>> {
    wit_signature(ctx, function)
        .map(|signature| TypedHandle::<FunctionType>::from_handle(signature, ctx))
        .transpose()
}

/// The integer domains of `signature`'s arguments and results (`None` where not an integer).
fn integer_signature(
    ctx: &Context,
    signature: TypedHandle<FunctionType>,
) -> (Vec<Option<Integer>>, Vec<Option<Integer>>) {
    let signature = signature.deref(ctx);
    let integers = |types: Vec<TypeHandle>| {
        types
            .into_iter()
            .map(|ty| Integer::from_type(ctx, ty))
            .collect()
    };
    (
        integers(signature.arg_types()),
        integers(signature.res_types()),
    )
}

/// The declared WIT integer domain at `position`, if the signature has one there.
fn declared(domains: &[Option<Integer>], position: usize) -> Option<Integer> {
    domains.get(position).copied().flatten()
}

/// Functions the host or the runtime enters: exports, the module initializer and `main`.
fn is_entry(ctx: &Context, function: &FuncOp) -> bool {
    let name = function.get_symbol_name(ctx);
    crate::calls::is_exported(ctx, function)
        || name.as_ref() == crate::globals::INITIALIZER
        || name.as_ref() == "perry_main"
}

/// Implementation selected for a constrained boundary, or the original function.
pub fn specialized_implementation(ctx: &Context, function: &FuncOp) -> Identifier {
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
    numeric_args: Vec<bool>,
    results: Vec<ResultDomain>,
    /// Actual MIR ranges never assume a specialization that has not been rewritten yet.
    specialize: bool,
}

impl Key {
    /// The context of `function` for integer argument and result domains, no numeric arguments.
    fn new(
        function: Identifier,
        args: Vec<Option<Integer>>,
        results: Vec<Option<Integer>>,
        specialize: bool,
    ) -> Self {
        Self {
            function,
            numeric_args: vec![false; args.len()],
            args,
            results: results.into_iter().map(ResultDomain::from).collect(),
            specialize,
        }
    }
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
    /// The function's own type, resolved once so later phases need not re-check it.
    signature: TypedHandle<FunctionType>,
    constraints: Vec<Constraint>,
    dependents: HashMap<Value, Vec<usize>>,
    parameters: Vec<Value>,
    returns: Vec<Ptr<Operation>>,
    declared_args: Vec<Option<Integer>>,
    declared_results: Vec<Option<Integer>>,
    effects: crate::effects::LocalEffects,
    external: bool,
    has_integer_constraints: bool,
}

impl Body {
    fn new(ctx: &Context, function: FuncOp, effects: crate::effects::LocalEffects) -> Result<Self> {
        let fn_ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)?;
        let mut constraints = Vec::new();
        let mut dependents: HashMap<Value, Vec<usize>> = HashMap::new();
        let mut returns = Vec::new();
        let signature = wit_function_type(ctx, &function)?;
        let exported = crate::calls::is_exported(ctx, &function);
        let mut has_integer_constraints = !exported && signature.is_some();
        for op in crate::aggregates::operations(ctx, &function) {
            let index = constraints.len();
            constraints.push(Constraint::Op {
                op,
                heap: heap::OperationKind::classify(ctx, op),
            });
            if Operation::get_op::<crate::control::ReturnOp>(op, ctx).is_some() {
                returns.push(op);
            }
            if !has_integer_constraints
                && ((Operation::get_op::<NumericCastOp>(op, ctx).is_some()
                    && Integer::from_type(ctx, op.deref(ctx).get_result(0).get_type(ctx))
                        .is_some())
                    || crate::intrinsics::intrinsic_of(ctx, op)
                        == Some(crate::intrinsics::Intrinsic::ParseInt)
                    || op
                        .deref(ctx)
                        .operands()
                        .chain(op.deref(ctx).results())
                        .any(|v| v.get_type(ctx).deref(ctx).is::<crate::types::BigIntType>()))
            {
                has_integer_constraints = true;
            }
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
        let (declared_args, declared_results) = match signature {
            Some(signature) => integer_signature(ctx, signature),
            None => {
                let ty = fn_ty.deref(ctx);
                (
                    vec![None; ty.arg_types().len()],
                    vec![None; ty.res_types().len()],
                )
            }
        };
        Ok(Self {
            external: exported || signature.is_some(),
            function,
            signature: fn_ty,
            effects,
            constraints,
            dependents,
            parameters: function
                .get_entry_block(ctx)
                .deref(ctx)
                .arguments()
                .collect(),
            returns,
            declared_args,
            declared_results,
            has_integer_constraints,
        })
    }
}

/// Specialized contexts per function before further keys share one summary context, so a
/// function has one generic context and at most `MAX_PRECISE_CONTEXTS + 1` specialized ones.
const MAX_PRECISE_CONTEXTS: usize = 32;
/// Allocation sites an alias set holds before it collapses to a typed summary.
const MAX_ORIGINS: usize = 32;

std::thread_local! {
    static FACT_LIMITS: Cell<(usize, usize)> = const {
        Cell::new((MAX_ORIGINS, MAX_PRECISE_CONTEXTS))
    };
}

fn max_origins() -> usize {
    FACT_LIMITS.get().0
}

fn max_precise_contexts() -> usize {
    FACT_LIMITS.get().1
}

/// Temporarily overrides `(MAX_ORIGINS, MAX_PRECISE_CONTEXTS)` on the current thread
/// for metamorphic testing of precision summarization vs. exact specialization.
#[doc(hidden)]
pub fn with_fact_limits<R>(
    max_origins: usize,
    max_precise_contexts: usize,
    f: impl FnOnce() -> R,
) -> R {
    /// Restores the previous limits however `f` exits.
    struct Restore((usize, usize));
    impl Drop for Restore {
        fn drop(&mut self) {
            FACT_LIMITS.set(self.0);
        }
    }
    let _restore = Restore(FACT_LIMITS.replace((max_origins, max_precise_contexts)));
    f()
}

#[derive(Default)]
struct State {
    facts: HashMap<Value, Fact>,
    ranges: HashMap<Value, Range>,
    range_expansions: HashMap<Value, u8>,
    required: HashMap<Value, Integer>,
    constrained: HashSet<Value>,
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
    fn is_constrained(&self, value: Value) -> bool {
        self.constrained.contains(&value)
    }
    fn representation(&self, value: Value) -> Option<Integer> {
        match self.fact(value) {
            Fact::Integer(integer) => Some(integer),
            Fact::Literal { .. } => self.required.get(&value).copied(),
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
    pub(crate) constraint_visits: usize,
    /// Alias sets collapsed to a typed summary after exceeding 32 allocation sites.
    pub(crate) summarized_heap_sets: usize,
    /// Specialized call contexts collapsed to a summary context after exceeding 32 precise contexts per function.
    pub(crate) summarized_contexts: usize,
    /// Value or result intervals widened to domain bounds after expanding more than once.
    range_widenings: usize,
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

    /// Generic and specialized context counts of `function`, for testing the context budget.
    #[cfg(test)]
    pub(crate) fn context_counts(&self, function: &Identifier) -> (usize, usize) {
        self.keys.iter().filter(|k| &k.function == function).fold(
            (0, 0),
            |(generic, specialized), key| {
                if key.specialize {
                    (generic, specialized + 1)
                } else {
                    (generic + 1, specialized)
                }
            },
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Work {
    Operation(usize, usize),
    OpenObject(usize),
    ReconcileCell(usize),
}

#[derive(Default)]
struct Worklist {
    pending: VecDeque<Work>,
    operations: Vec<Vec<bool>>,
    objects: Vec<bool>,
    cells: Vec<bool>,
}

impl Worklist {
    /// Whether `item` is pending.
    fn queued(&mut self, item: Work) -> &mut bool {
        match item {
            Work::Operation(context, index) => &mut self.operations[context][index],
            Work::OpenObject(object) => &mut self.objects[object],
            Work::ReconcileCell(cell) => &mut self.cells[cell],
        }
    }

    fn enqueue(&mut self, item: Work) {
        if !std::mem::replace(self.queued(item), true) {
            self.pending.push_back(item);
        }
    }

    fn pop(&mut self) -> Option<Work> {
        let item = self.pending.pop_front()?;
        *self.queued(item) = false;
        Some(item)
    }
}

struct Solver {
    analysis: ProgramFacts,
    contexts: HashMap<Key, usize>,
    precise_counts: HashMap<Identifier, usize>,
    summary_contexts: HashMap<Identifier, usize>,
    work: Worklist,
    width_defaults: Vec<(usize, usize, Value)>,
}

impl Solver {
    /// Whether context `id` is the shared summary of its function's surplus specializations.
    fn is_summary(&self, id: usize) -> bool {
        self.summary_contexts.get(&self.analysis.keys[id].function) == Some(&id)
    }

    fn summary_key(&self, ctx: &Context, key: &Key) -> Key {
        let body = &self.analysis.bodies[&key.function];
        let is_async = crate::calls::is_async(ctx, &body.function);
        let mut args = vec![None; key.args.len()];
        let mut numeric_args = vec![false; key.args.len()];
        for (idx, param) in body.parameters.iter().enumerate() {
            let ty = param.get_type(ctx);
            args[idx] = declared(&body.declared_args, idx).or_else(|| Integer::from_type(ctx, ty));
            // Unconstrained numbers stay lossless unless an async payload keeps its carrier.
            let number = crate::optional::optional_value(ctx, ty).unwrap_or(ty);
            numeric_args[idx] = args[idx].is_none()
                && !is_async
                && number.deref(ctx).is::<pliron::builtin::types::FP64Type>();
        }
        let res_types = body.signature.deref(ctx).res_types();
        let results = (0..key.results.len())
            .map(|idx| {
                let integer = declared(&body.declared_results, idx)
                    .or_else(|| Integer::from_type(ctx, *res_types.get(idx)?));
                let unknown = if is_async {
                    ResultDomain::Original
                } else {
                    ResultDomain::Inferred
                };
                integer.map_or(unknown, ResultDomain::Integer)
            })
            .collect();
        Key {
            function: key.function.clone(),
            args,
            numeric_args,
            results,
            specialize: true,
        }
    }

    fn context(&mut self, ctx: &Context, key: Key) -> usize {
        if let Some(&id) = self.contexts.get(&key) {
            return id;
        }
        if key.specialize {
            if let Some(&id) = self.summary_contexts.get(&key.function) {
                self.analysis.summarized_contexts += 1;
                self.contexts.insert(key, id);
                return id;
            }
            let count = self.precise_counts.entry(key.function.clone()).or_insert(0);
            if *count >= max_precise_contexts() {
                self.analysis.summarized_contexts += 1;
                let summary = self.summary_key(ctx, &key);
                let id = if let Some(&id) = self.contexts.get(&summary) {
                    id
                } else {
                    let id = self.allocate_context(ctx, summary.clone());
                    self.contexts.insert(summary, id);
                    id
                };
                self.summary_contexts.insert(key.function.clone(), id);
                self.contexts.insert(key, id);
                return id;
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
        for &op in &body.returns {
            for (position, value) in op.deref(ctx).operands().enumerate() {
                if let Some(required) = key.results.get(position).and_then(|r| r.integer()) {
                    let entry = state.required.entry(value).or_insert(required);
                    *entry = entry.common(required);
                    state.constrained.insert(value);
                }
            }
        }
        loop {
            let mut progressed = false;
            for constraint in &body.constraints {
                if let Constraint::Edge { from, to } = *constraint {
                    if let Some(&req) = state.required.get(&to) {
                        let old = state.required.get(&from).copied();
                        let new = old.map_or(req, |old| old.common(req));
                        if old != Some(new) {
                            state.required.insert(from, new);
                            progressed = true;
                        }
                    }
                    if state.constrained.contains(&to) && state.constrained.insert(from) {
                        progressed = true;
                    }
                }
            }
            if !progressed {
                break;
            }
        }
        for (position, (&parameter, domain)) in body.parameters.iter().zip(&key.args).enumerate() {
            if body.external {
                state
                    .outcomes
                    .insert(parameter, Outcomes::unknown(ctx, parameter.get_type(ctx)));
            }
            if crate::types::is_reference(ctx, parameter.get_type(ctx)) {
                state.origins.insert(parameter, BTreeSet::from([position]));
            }
            let fact = if let Some(domain) = *domain {
                Fact::Integer(domain)
            } else if key.numeric_args.get(position).copied().unwrap_or(false) {
                Fact::Numeric
            } else {
                Fact::Original
            };
            state.facts.insert(parameter, fact);
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
                        Ok((
                            function.get_symbol_name(ctx),
                            Body::new(ctx, function, effects)?,
                        ))
                    })
                    .collect::<Result<_>>()?,
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
            summary_contexts: HashMap::new(),
            work: Worklist::default(),
            width_defaults: Vec::new(),
        };
        let has_integer_constraints = solver
            .analysis
            .bodies
            .values()
            .any(|body| body.has_integer_constraints);
        for function in functions {
            let name = function.get_symbol_name(ctx);
            let ty = solver.analysis.bodies[&name].signature;
            let (args, results) = integer_signature(ctx, ty);
            let generic = solver.context(ctx, Key::new(name.clone(), args, results, false));
            solver.analysis.generic.insert(name.clone(), generic);
            if function
                .get_operation()
                .deref(ctx)
                .attributes
                .0
                .contains_key(&SPECIALIZATION)
            {
                continue;
            }
            let signature = wit_function_type(ctx, &function)?;
            if signature.is_none() && !(has_integer_constraints && is_entry(ctx, &function)) {
                continue;
            }
            let key = if let Some(signature) = signature {
                let (args, results) = integer_signature(ctx, signature);
                Key::new(name, args, results, true)
            } else {
                // Unconstrained entry results keep their representation.
                let (args, results) = integer_signature(ctx, ty);
                let mut key = Key::new(name, args, results, true);
                for result in &mut key.results {
                    if *result == ResultDomain::Inferred {
                        *result = ResultDomain::Original;
                    }
                }
                key
            };
            let id = solver.context(ctx, key);
            solver.analysis.roots.push((function, id));
        }
        loop {
            while let Some(item) = solver.work.pop() {
                solver.analysis.constraint_visits += 1;
                match item {
                    Work::Operation(id, index) => solver.process(ctx, id, index),
                    Work::OpenObject(object) => solver.open_object(ctx, object),
                    Work::ReconcileCell(cell) => solver.reconcile_cell(ctx, cell),
                }
            }
            // Every default is applied after propagation settles, before the next queue drain.
            solver
                .width_defaults
                .sort_unstable_by_key(|&(id, index, _)| std::cmp::Reverse((id, index)));
            let mut defaulted = false;
            while let Some((id, _, value)) = solver.width_defaults.pop() {
                solver.analysis.constraint_visits += 1;
                if !solver.analysis.states[id].required.contains_key(&value) {
                    let default_int = solver.analysis.states[id]
                        .fact(value)
                        .integer()
                        .map(Integer::unconstrained)
                        .or_else(|| {
                            value
                                .get_type(ctx)
                                .deref(ctx)
                                .is::<crate::types::BigIntType>()
                                .then_some(Integer::I64)
                        });
                    if let Some(integer) = default_int {
                        solver.require(id, value, Some(integer));
                        defaulted = true;
                    }
                }
            }
            if !defaulted {
                break;
            }
        }
        Ok(solver.analysis)
    }
}
