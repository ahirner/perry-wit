//! Cached numeric, runtime-effect and parameter-provenance constraints over SSA values.
//!
//! Contexts are keyed by argument and result domains. A single worklist propagates
//! representation facts forward and required widths backward, including recursive calls.
//! Rewriting runs after the analysis converges, fixing semantic wrap points before
//! SCCP, inlining and other algebraic transformations.
use crate::{calls::AwaitOp, scalar::NumericCastOp, semantics::OperationKind, types::PromiseType};
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
    utils::table::HSet,
    value::Value,
};
use std::{
    cell::Cell,
    collections::{BTreeSet, HashMap, VecDeque},
};

mod domains;
mod heap;
pub(crate) mod paths;
mod rewrite;
mod transfer;
use domains::{
    Fact, Integer, Interval, Range, ResultDomain, arithmetic_range, comparison_domain,
    constant_range, integer_arithmetic, literal,
};
#[cfg(test)]
pub(crate) use rewrite::verify_lossless_module;
pub(crate) use rewrite::{specialize, verify_lossless_operations};
mod value;
use value::{Demand, Representation, ValueFacts, ValueState};
mod outcomes;
use outcomes::Outcomes;

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

/// Each context argument has exactly one representation constraint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ArgumentDomain {
    Original,
    Integer(Integer),
    Numeric,
}

impl ArgumentDomain {
    fn integer(self) -> Option<Integer> {
        match self {
            Self::Integer(integer) => Some(integer),
            Self::Original | Self::Numeric => None,
        }
    }
}

impl From<Option<Integer>> for ArgumentDomain {
    fn from(integer: Option<Integer>) -> Self {
        integer.map_or(Self::Original, Self::Integer)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    function: Identifier,
    args: Vec<ArgumentDomain>,
    results: Vec<ResultDomain>,
    /// Whether this context represents a specialized clone of `function`.
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
            args: args.into_iter().map(ArgumentDomain::from).collect(),
            results: results.into_iter().map(ResultDomain::from).collect(),
            specialize,
        }
    }
}

#[derive(Clone, Copy)]
enum Constraint {
    Op {
        op: Ptr<Operation>,
        kind: OperationKind,
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
            let kind = OperationKind::classify(ctx, op);
            constraints.push(Constraint::Op { op, kind });
            if kind == OperationKind::Return {
                returns.push(op);
            }
            if !has_integer_constraints
                && ((Operation::get_op::<NumericCastOp>(op, ctx).is_some()
                    && Integer::from_type(ctx, op.deref(ctx).get_result(0).get_type(ctx))
                        .is_some())
                    || crate::intrinsics::intrinsic_of(ctx, op).is_some_and(|intrinsic| {
                        intrinsic == crate::intrinsics::Intrinsic::ParseInt
                            || intrinsic.int32_operands()
                    })
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
    values: HashMap<Value, ValueState>,
    calls: HashMap<Ptr<Operation>, usize>,
    results: Vec<ResultFact>,
    callers: BTreeSet<(usize, usize)>,
    origins: HashMap<Value, BTreeSet<usize>>,
    summary: crate::effects::Summary,
    heap_values: HashMap<Value, usize>,
}

#[derive(Clone, Copy, Default)]
struct ResultFact {
    facts: ValueFacts,
    heap: Option<usize>,
}

impl State {
    fn demand(&mut self, value: Value, incoming: Demand) -> bool {
        incoming != Demand::None && self.values.entry(value).or_default().require(incoming)
    }

    fn value(&self, value: Value) -> ValueFacts {
        self.values
            .get(&value)
            .map_or(ValueFacts::default(), ValueState::facts)
    }
    fn fact(&self, value: Value) -> Fact {
        self.value(value).domain
    }
    fn range(&self, value: Value) -> Range {
        self.value(value).range
    }
    fn usage(&self, value: Value) -> Demand {
        self.values
            .get(&value)
            .map_or(Demand::None, ValueState::demand)
    }
    fn is_constrained(&self, value: Value) -> bool {
        matches!(self.usage(value), Demand::Truncated(_))
    }
    fn preserves_original(&self, value: Value) -> bool {
        self.usage(value) == Demand::Original
    }
    fn width(&self, value: Value) -> Option<Integer> {
        self.usage(value).width().map(Integer::register)
    }

    fn representation(&self, value: Value) -> Option<Integer> {
        match self.fact(value) {
            Fact::Integer(integer) => Some(integer),
            // A literal takes a demanded width only when every value it can hold fits.
            Fact::Literal { .. } => {
                let facts = self.value(value);
                self.width(value).filter(|&required| {
                    !facts.inexact
                        && match facts.range {
                            Range::Bounds(interval) => interval.fits(required),
                            Range::Bottom => true,
                            Range::Unknown => false,
                        }
                })
            }
            _ => None,
        }
    }
}

/// Cached analysis result for an unchanged module. Domain propagation implements Perry
/// semantics on top of pliron scheduling, cloning, verification, and invalidation infrastructure.
pub struct ProgramFacts {
    bodies: HashMap<Identifier, Body>,
    keys: Vec<Key>,
    states: Vec<State>,
    roots: Vec<(FuncOp, usize)>,
    generic: HashMap<Ptr<Operation>, usize>,
    pub(crate) constraint_visits: usize,
    /// Alias sets collapsed to a typed summary after exceeding 32 allocation sites.
    pub(crate) summarized_heap_sets: usize,
    /// Specialized call contexts collapsed to a summary context after exceeding 32 precise contexts per function.
    pub(crate) summarized_contexts: usize,
    /// Value or result intervals widened to domain bounds after expanding more than once.
    range_widenings: usize,
    heap: heap::HeapFacts,
}

#[derive(Debug)]
struct MissingEffects(Identifier);

impl core::fmt::Display for MissingEffects {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "function {} has no effect summary", self.0)
    }
}

impl core::error::Error for MissingEffects {}

impl ProgramFacts {
    fn generic_context(&self, name: &Identifier) -> Option<usize> {
        let function = self.bodies.get(name)?.function.get_operation();
        self.generic.get(&function).copied()
    }

    pub fn effects(&self, function: &Identifier) -> Option<&crate::effects::Summary> {
        self.generic_context(function)
            .map(|id| &self.states[id].summary)
    }

    /// A resolved function must have been included in this analysis.
    pub fn function_summary(
        &self,
        ctx: &Context,
        function: &FuncOp,
    ) -> Result<&crate::effects::Summary> {
        self.generic.get(&function.get_operation()).map_or_else(
            || {
                pliron::input_err!(
                    function.loc(ctx).clone(),
                    MissingEffects(function.get_symbol_name(ctx))
                )
            },
            |&id| Ok(&self.states[id].summary),
        )
    }

    /// The interval `value` lies in under every call context of its function, when proven.
    /// A bounded value is integral even if its type is a number: intervals start from integer
    /// constants and grow only through `+`, `-` and `*` of bounded operands.
    pub fn integral_interval(&self, ctx: &Context, value: Value) -> Option<(i128, i128)> {
        let mut parent = value.get_parent_block(ctx)?.deref(ctx).get_parent_op(ctx);
        while let Some(op) = parent {
            if let Some(&context) = self.generic.get(&op) {
                return match self.states.get(context)?.range(value) {
                    Range::Bounds(Interval { lower, upper }) => Some((lower, upper)),
                    Range::Bottom | Range::Unknown => None,
                };
            }
            parent = op.deref(ctx).get_parent_op(ctx);
        }
        None
    }

    /// Non-negative integral values, including loop-carried counts after range widening.
    pub(crate) fn is_count(&self, ctx: &Context, function: &FuncOp, value: Value) -> bool {
        let state = &self.states[self.generic[&function.get_operation()]];
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Work {
    Operation(usize, usize),
    OpenObject(usize),
    ReconcileCell(usize),
}

#[derive(Default)]
struct Worklist {
    pending: VecDeque<Work>,
    queued: HSet<Work>,
}

impl Worklist {
    fn insert(&mut self, work: Work) {
        if self.queued.insert(work) {
            self.pending.push_back(work);
        }
    }

    fn pop(&mut self) -> Option<Work> {
        let work = self.pending.pop_front()?;
        self.queued.remove(&work);
        Some(work)
    }
}

struct Solver {
    analysis: ProgramFacts,
    contexts: HashMap<Key, usize>,
    precise_counts: HashMap<Identifier, usize>,
    summary_contexts: HashMap<Identifier, usize>,
    work: Worklist,
}

impl Solver {
    /// Whether context `id` is the shared summary of its function's surplus specializations.
    fn is_summary(&self, id: usize) -> bool {
        self.summary_contexts.get(&self.analysis.keys[id].function) == Some(&id)
    }

    fn summary_key(&self, ctx: &Context, key: &Key) -> Key {
        let body = &self.analysis.bodies[&key.function];
        let is_async = crate::calls::is_async(ctx, &body.function);
        let args = body
            .parameters
            .iter()
            .enumerate()
            .map(|(idx, param)| {
                let ty = param.get_type(ctx);
                if let Some(integer) =
                    declared(&body.declared_args, idx).or_else(|| Integer::from_type(ctx, ty))
                {
                    return ArgumentDomain::Integer(integer);
                }
                // Unconstrained numbers stay lossless unless an async payload keeps its carrier.
                let number = crate::optional::present_type(ctx, ty);
                if !is_async && number.deref(ctx).is::<pliron::builtin::types::FP64Type>() {
                    ArgumentDomain::Numeric
                } else {
                    ArgumentDomain::Original
                }
            })
            .collect();
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
            results,
            specialize: true,
        }
    }

    fn context(&mut self, ctx: &Context, key: Key) -> Result<usize> {
        if let Some(&id) = self.contexts.get(&key) {
            return Ok(id);
        }
        if key.specialize {
            if let Some(&id) = self.summary_contexts.get(&key.function) {
                self.analysis.summarized_contexts += 1;
                self.contexts.insert(key, id);
                return Ok(id);
            }
            let count = self.precise_counts.entry(key.function.clone()).or_insert(0);
            if *count >= max_precise_contexts() {
                self.analysis.summarized_contexts += 1;
                let summary = self.summary_key(ctx, &key);
                let id = match self.contexts.get(&summary).copied() {
                    Some(id) => id,
                    None => self.allocate_context(ctx, summary)?,
                };
                self.summary_contexts.insert(key.function.clone(), id);
                self.contexts.insert(key, id);
                return Ok(id);
            }
            *count += 1;
        }
        self.allocate_context(ctx, key)
    }

    fn allocate_context(&mut self, ctx: &Context, key: Key) -> Result<usize> {
        let id = self.analysis.keys.len();
        self.contexts.insert(key.clone(), id);
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
            if crate::types::is_reference(ctx, parameter.get_type(ctx)) {
                state.origins.insert(parameter, BTreeSet::from([position]));
            }
            let fact = match *domain {
                ArgumentDomain::Integer(integer) => Fact::Integer(integer),
                ArgumentDomain::Numeric => Fact::Numeric,
                ArgumentDomain::Original => Fact::Original,
            };
            let mut facts = ValueFacts::numeric(
                fact,
                domain.integer().map_or(
                    if body.external {
                        Range::Unknown
                    } else {
                        Range::Bottom
                    },
                    |domain| Range::Bounds(domain.bounds()),
                ),
            );
            if body.external {
                facts.outcome = Outcomes::unknown(ctx, parameter.get_type(ctx));
            }
            state.values.insert(parameter, ValueState::new(facts));
        }
        let constraints = body.constraints.len();
        self.analysis.keys.push(key);
        self.analysis.states.push(state);
        for constraint in 0..constraints {
            self.work.insert(Work::Operation(id, constraint));
        }
        self.seed_heap_parameters(ctx, id)?;
        // The signature is fixed for this context, even when requirements propagate
        // through the parameter: conversions belong at its uses, not at its entry.
        Ok(id)
    }

    fn changed(&mut self, id: usize, value: Value) {
        let body = &self.analysis.bodies[&self.analysis.keys[id].function];
        let dependents = body.dependents.get(&value).into_iter().flatten();
        let parameter = body.parameters.contains(&value);
        for &constraint in dependents {
            self.work.insert(Work::Operation(id, constraint));
        }
        if parameter {
            self.notify_callers(id);
        }
    }

    fn notify_callers(&mut self, id: usize) {
        for &caller in &self.analysis.states[id].callers {
            self.work.insert(Work::Operation(caller.0, caller.1));
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
            let generic = solver.context(ctx, Key::new(name.clone(), args, results, false))?;
            solver
                .analysis
                .generic
                .insert(function.get_operation(), generic);
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
            let id = solver.context(ctx, key)?;
            solver.analysis.roots.push((function, id));
        }
        while let Some(item) = solver.work.pop() {
            solver.analysis.constraint_visits += 1;
            match item {
                Work::Operation(id, index) => solver.process(ctx, id, index)?,
                Work::OpenObject(object) => solver.open_object(ctx, object)?,
                Work::ReconcileCell(cell) => solver.reconcile_cell(ctx, cell),
            }
        }
        Ok(solver.analysis)
    }
}
