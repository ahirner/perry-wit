//! Alias, JSON-residue, and intervening-write queries over converged `HeapFacts`.
use super::*;
use crate::effects::{Mutation, Observed};
use pliron::builtin::op_interfaces::OneOpdInterface;
use std::collections::HashSet;

/// The heap cell `value` names, looking through operations whose result aliases an operand,
/// and whether it may hold an untracked aggregate. `visited` terminates alias cycles in
/// unreachable code as untracked.
fn root_heap_cells(ctx: &Context, state: &State, value: Value) -> (Vec<usize>, bool) {
    let mut visited = HashSet::new();
    let mut current = value;
    loop {
        if let Some(&cell) = state.heap_values.get(&current) {
            return (vec![cell], false);
        }
        let aliased = current.defining_op().and_then(|op| {
            let operand = OperationKind::classify(ctx, op).aliased_operand()?;
            visited
                .insert(op)
                .then(|| op.deref(ctx).get_operand(operand))
        });
        match aliased {
            Some(operand) => current = operand,
            None => {
                let untracked = crate::effects::may_hold_heap(ctx, current.get_type(ctx));
                return (Vec::new(), untracked);
            }
        }
    }
}

/// The objects reachable from `root_cells`, and whether any of them may hold unmodelled
/// contents: an untracked root, an empty or summarized cell, or an open object.
fn collect_reachable_heap(
    heap: &HeapFacts,
    root_cells: &[usize],
    untracked: bool,
) -> (BTreeSet<usize>, bool) {
    let cells = crate::reachable(root_cells.iter().copied(), move |cell| {
        heap.cells[cell]
            .objects
            .iter()
            .flat_map(move |&object| heap.objects[object].contents.slots().iter().copied())
    });
    let objects: BTreeSet<usize> = cells
        .iter()
        .flat_map(|&cell| heap.cells[cell].objects.iter().copied())
        .collect();
    let open = untracked
        || root_cells
            .iter()
            .any(|&cell| heap.cells[cell].objects.is_empty())
        || cells.iter().any(|&cell| heap.cells[cell].summarized)
        || objects.iter().any(|&object| heap.objects[object].open);
    (objects, open)
}

struct Aliases<'a> {
    ctx: &'a Context,
    heap: &'a HeapFacts,
    objects: BTreeSet<usize>,
    open: bool,
}

impl<'a> Aliases<'a> {
    fn new(ctx: &'a Context, heap: &'a HeapFacts, state: &State, value: Value) -> Self {
        let (cells, untracked) = root_heap_cells(ctx, state, value);
        let (objects, open) = collect_reachable_heap(heap, &cells, untracked);
        Self {
            ctx,
            heap,
            objects,
            open,
        }
    }

    fn contains(&self, state: &State, value: Value) -> bool {
        let (cells, untracked) = root_heap_cells(self.ctx, state, value);
        let (objects, open) = collect_reachable_heap(self.heap, &cells, untracked);
        !self.objects.is_disjoint(&objects) || self.open && open
    }

    /// Whether `op` of `kind` in the function of `state`, other than a call, may change the
    /// `observed` state of these objects. Other tasks and host code run during a suspension or
    /// a foreign call.
    fn changed_by(
        &self,
        observed: Observed,
        state: &State,
        op: Ptr<Operation>,
        kind: OperationKind,
    ) -> bool {
        let operation = op.deref(self.ctx);
        match kind {
            OperationKind::Mutate(mutation) => {
                observed.changed_by(mutation)
                    && (operation.operands().next())
                        .is_none_or(|object| self.contains(state, object))
            }
            OperationKind::Await | OperationKind::Foreign(_) => true,
            _ => {
                crate::effects::writes_operands(self.ctx, op, kind)
                    && operation
                        .operands()
                        .any(|value| self.contains(state, value))
            }
        }
    }
}

/// What a client read: state reachable from a value's objects, or a module binding.
enum Tracked<'a> {
    Objects(&'a State, Aliases<'a>),
    Global(Identifier),
    /// The function has no analyzed context.
    Unknown,
}

/// Whether an operation may change what a client read.
/// Unanalyzed functions and callees invalidate every read.
struct Interference<'a> {
    facts: &'a ProgramFacts,
    ctx: &'a Context,
    observed: Observed,
    tracked: Tracked<'a>,
    calls: HashMap<usize, bool>,
}

/// The module binding a global slot names.
fn global(ctx: &Context, slot: Value) -> Option<Identifier> {
    Some(Operation::get_op::<crate::GlobalOp>(slot.defining_op()?, ctx)?.name(ctx))
}

impl Interference<'_> {
    fn invalidates(&mut self, op: Ptr<Operation>) -> bool {
        let (state, aliases) = match &self.tracked {
            Tracked::Unknown => return true,
            Tracked::Global(name) => {
                // Only a store to its slot writes a binding; callees summarize theirs.
                return match OperationKind::classify(self.ctx, op) {
                    OperationKind::Mutate(Mutation::Local) => {
                        (op.deref(self.ctx).operands().next())
                            .is_none_or(|slot| global(self.ctx, slot).as_ref() == Some(name))
                    }
                    OperationKind::Call { .. } => crate::calls::callee(self.ctx, op)
                        .and_then(|callee| self.facts.effects(&callee))
                        .is_none_or(|summary| summary.effects.may_change(self.observed)),
                    OperationKind::Await | OperationKind::Foreign(_) => true,
                    _ => false,
                };
            }
            Tracked::Objects(state, aliases) => (state, aliases),
        };
        if aliases.objects.is_empty() && !aliases.open {
            return false;
        }
        let kind = OperationKind::classify(self.ctx, op);
        if !matches!(kind, OperationKind::Call { .. }) {
            return aliases.changed_by(self.observed, state, op, kind);
        }
        let Some(callee) =
            crate::calls::callee(self.ctx, op).and_then(|name| self.facts.generic_context(&name))
        else {
            return true;
        };
        if let Some(&changes) = self.calls.get(&callee) {
            return changes;
        }
        let changes = self
            .facts
            .call_changes(self.ctx, callee, aliases, self.observed);
        self.calls.insert(callee, changes);
        changes
    }
}

impl ProgramFacts {
    fn interference<'a>(
        &'a self,
        ctx: &'a Context,
        function: Ptr<Operation>,
        value: Value,
        observed: Observed,
    ) -> Interference<'a> {
        let tracked = match (observed, self.generic.get(&function)) {
            (_, None) => Tracked::Unknown,
            (Observed::Binding, Some(_)) => {
                global(ctx, value).map_or(Tracked::Unknown, Tracked::Global)
            }
            (_, Some(&id)) => {
                let state = &self.states[id];
                Tracked::Objects(state, Aliases::new(ctx, &self.heap, state, value))
            }
        };
        Interference {
            facts: self,
            ctx,
            observed,
            tracked,
            calls: HashMap::new(),
        }
    }

    /// Whether calling the function of context `root` may change the `observed` state of
    /// `aliases`, through its own operations or its callees'.
    fn call_changes(
        &self,
        ctx: &Context,
        root: usize,
        aliases: &Aliases<'_>,
        observed: Observed,
    ) -> bool {
        let mut pending = vec![root];
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let state = &self.states[id];
            let effects = state.summary.effects;
            if effects.may_suspend || effects.may_reenter {
                return true;
            }
            if !effects.may_change(observed) {
                continue;
            }
            for constraint in &self.bodies[&self.keys[id].function].constraints {
                let Constraint::Op { op, kind } = *constraint else {
                    continue;
                };
                if !matches!(kind, OperationKind::Call { .. }) {
                    if aliases.changed_by(observed, state, op, kind) {
                        return true;
                    }
                    continue;
                }
                let Some(child) =
                    crate::calls::callee(ctx, op).and_then(|name| self.generic_context(&name))
                else {
                    return true;
                };
                pending.push(child);
            }
        }
        false
    }

    pub(in crate::facts) fn same_read(&self, ctx: &Context, earlier: Value, later: Value) -> bool {
        if earlier == later {
            return true;
        }
        let Some((read, repeated)) = earlier.defining_op().zip(later.defining_op()) else {
            return false;
        };
        let (source, observed) = if let Some((field, repeated_field)) =
            Operation::get_op::<FieldGetOp>(read, ctx)
                .zip(Operation::get_op::<FieldGetOp>(repeated, ctx))
            && let source = field.get_operand(ctx)
            && source == repeated_field.get_operand(ctx)
            && field.field(ctx) == repeated_field.field(ctx)
        {
            (source, Observed::Fields)
        } else if let Some((load, repeated_load)) =
            Operation::get_op::<crate::GlobalLoadOp>(read, ctx)
                .zip(Operation::get_op::<crate::GlobalLoadOp>(repeated, ctx))
            && let source = load.get_operand(ctx)
            && global(ctx, source)
                .is_some_and(|name| global(ctx, repeated_load.get_operand(ctx)) == Some(name))
        {
            (source, Observed::Binding)
        } else {
            return false;
        };
        let Some(function) = read.deref(ctx).get_parent_op(ctx) else {
            return false;
        };
        let mut interference = self.interference(ctx, function, source, observed);
        super::super::paths::preserves(ctx, read, repeated, (), |op, ()| {
            (!interference.invalidates(op)).then_some(())
        })
    }

    pub(crate) fn array_length_invalidation<'a>(
        &'a self,
        ctx: &'a Context,
        function: &FuncOp,
        array: Value,
    ) -> impl FnMut(Ptr<Operation>) -> bool + 'a {
        let mut interference =
            self.interference(ctx, function.get_operation(), array, Observed::Length);
        move |op| interference.invalidates(op)
    }

    /// Whether `value` in `function` may originate from an open/unmodelled JSON source
    /// that could carry extra unvalidated JSON fields beyond its static MIR type.
    pub(crate) fn may_have_json_residue(
        &self,
        ctx: &Context,
        function: &FuncOp,
        value: Value,
    ) -> bool {
        let Some(&id) = self.generic.get(&function.get_operation()) else {
            return true;
        };
        let state = &self.states[id];
        let (root_cells, untracked) = root_heap_cells(ctx, state, value);
        let (_, open_or_summarized) = collect_reachable_heap(&self.heap, &root_cells, untracked);
        open_or_summarized
    }

    /// Whether any operation on a path from the stringify to the consumer of `site` may change
    /// state reachable from its source, or is observable before an exception the rewrite moves.
    pub(crate) fn may_prevent_json_copy_between(
        &self,
        ctx: &Context,
        function: &FuncOp,
        site: &crate::json_copy::ParseStringifySite,
    ) -> bool {
        let parse = site.intermediate_parse_op;
        let mut interference = self.interference(
            ctx,
            function.get_operation(),
            site.source_value,
            Observed::Contents,
        );
        // Walking backwards, the state is whether `op` runs after a parse that may throw.
        !super::super::paths::preserves(
            ctx,
            site.stringify_op,
            site.consumer_op,
            parse.is_some_and(|parse| crate::may_throw(ctx, parse)),
            |op, parse_throws| {
                if Some(op) == site.unwrap_op {
                    return Some(parse_throws);
                }
                if Some(op) == parse {
                    return Some(false);
                }
                // A status read belongs to the operation before it.
                if Operation::is_op::<crate::exceptions::CaughtOp>(op, ctx) {
                    return Some(parse_throws);
                }
                // An exception the rewrite moves to the consumer would follow these effects.
                let reorders =
                    (site.text_may_throw || parse_throws) && crate::effects::observable(ctx, op);
                (!reorders && !interference.invalidates(op)).then_some(parse_throws)
            },
        )
    }
}
