//! Allocation identities and stable slot facts, independent of physical storage.
//! Writes join into slots; readers subscribe to slots and possible object identities.
use super::*;
use crate::{
    collections::{
        ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
        ArrayTruncateOp, ConcatOp, ConcatReuseOp, ConcatSegmentsOp,
    },
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{Combinator, PromiseCombinatorOp},
    optional::{OptionalCastOp, UnwrapOp, WrapOp},
    types::{ArrayType, RecordType},
};
use std::ops::Bound::{Excluded, Unbounded};

mod operations;
mod queries;
mod specialize;

pub(super) use operations::OperationKind;

struct Cell {
    ty: TypeHandle,
    objects: BTreeSet<usize>,
    layout_objects: BTreeSet<usize>,
    owner: Option<usize>,
    summarized: bool,
    domain: Fact,
    range: Range,
    outcome: Outcomes,
    readers: BTreeSet<Work>,
}

struct Object {
    ty: TypeHandle,
    contents: Contents,
    open: bool,
    containers: BTreeSet<usize>,
}

enum Contents {
    Record(Vec<usize>),
    Array {
        element: usize,
        length: Range,
        readers: BTreeSet<Work>,
    },
}

impl Contents {
    fn slots(&self) -> &[usize] {
        match self {
            Self::Record(slots) => slots,
            Self::Array { element, .. } => std::slice::from_ref(element),
        }
    }
}

#[derive(Default)]
pub(super) struct HeapFacts {
    cells: Vec<Cell>,
    objects: Vec<Object>,
    summaries: HashMap<TypeHandle, usize>,
}

impl HeapFacts {
    fn length(&self, base: usize) -> Range {
        self.cells[base]
            .objects
            .iter()
            .fold(Range::Bottom, |range, &object| {
                let Contents::Array { length, .. } = &self.objects[object].contents else {
                    return range;
                };
                match (range, *length) {
                    (Range::Bottom, range) | (range, Range::Bottom) => range,
                    (Range::Bounds(a), Range::Bounds(b)) => Range::Bounds(Interval {
                        lower: a.lower.min(b.lower),
                        upper: a.upper.max(b.upper),
                    }),
                    _ => Range::Unknown,
                }
            })
    }
}

impl ProgramFacts {
    pub(crate) fn array_length(
        &self,
        ctx: &Context,
        function: &FuncOp,
        array: Value,
    ) -> Option<(u32, u32)> {
        let state = &self.states[self.generic[&function.get_symbol_name(ctx)]];
        let base = *state.heap_values.get(&array)?;
        let Range::Bounds(length) = self.heap.length(base) else {
            return None;
        };
        Some((length.lower.try_into().ok()?, length.upper.try_into().ok()?))
    }

    pub(crate) fn array_in_bounds(
        &self,
        ctx: &Context,
        function: &FuncOp,
        array: Value,
        index: Value,
    ) -> bool {
        let Some((minimum, _)) = self.array_length(ctx, function, array) else {
            return false;
        };
        let state = &self.states[self.generic[&function.get_symbol_name(ctx)]];
        crate::optional::optional_value(ctx, index.get_type(ctx)).is_none()
            && matches!(state.range(index), Range::Bounds(Interval { lower: 0.., upper }) if upper < i128::from(minimum))
    }
}

fn payload(ctx: &Context, mut ty: TypeHandle) -> TypeHandle {
    loop {
        if let Some(inner) = crate::optional::optional_value(ctx, ty) {
            ty = inner;
        } else if let Some(promise) = ty.deref(ctx).downcast_ref::<PromiseType>() {
            ty = promise.value();
        } else {
            return ty;
        }
    }
}

/// The first element of `set` after `previous`, or its first.
fn after(set: &BTreeSet<usize>, previous: Option<usize>) -> Option<usize> {
    let start = previous.map_or(Unbounded, Excluded);
    set.range((start, Unbounded)).next().copied()
}

pub(super) fn aggregate(ctx: &Context, ty: TypeHandle) -> bool {
    let ty = payload(ctx, ty).deref(ctx);
    ty.is::<RecordType>() || ty.is::<ArrayType>()
}

impl Solver {
    /// A new cell of type `ty`: a slot of object `owner`, or an SSA value or result.
    fn heap_cell(&mut self, ty: TypeHandle, owner: Option<usize>) -> usize {
        let id = self.analysis.heap.cells.len();
        self.analysis.heap.cells.push(Cell {
            ty,
            objects: BTreeSet::new(),
            layout_objects: BTreeSet::new(),
            owner,
            summarized: false,
            domain: Fact::Bottom,
            range: Range::Bottom,
            outcome: Outcomes::Bottom,
            readers: BTreeSet::new(),
        });
        self.work.cells.push(false);
        id
    }

    fn heap_value(&mut self, ctx: &Context, id: usize, value: Value) -> Option<usize> {
        if !aggregate(ctx, value.get_type(ctx)) {
            return None;
        }
        if let Some(&cell) = self.analysis.states[id].heap_values.get(&value) {
            return Some(cell);
        }
        let cell = self.heap_cell(value.get_type(ctx), None);
        self.analysis.states[id].heap_values.insert(value, cell);
        Some(cell)
    }

    fn heap_object(&mut self, ctx: &Context, ty: TypeHandle, open: bool) -> usize {
        let id = self.analysis.heap.objects.len();
        let ty = payload(ctx, ty);
        let shape = ty.deref(ctx);
        let contents = if let Some(record) = shape.downcast_ref::<RecordType>() {
            Contents::Record(
                record
                    .fields()
                    .expect("resolved record schema")
                    .iter()
                    .map(|&(_, ty)| self.heap_cell(ty, Some(id)))
                    .collect(),
            )
        } else {
            let array = shape.downcast_ref::<ArrayType>().expect("aggregate schema");
            Contents::Array {
                element: self.heap_cell(array.element(), Some(id)),
                length: Range::Bottom,
                readers: BTreeSet::new(),
            }
        };
        let slots: Vec<usize> = contents.slots().to_vec();
        self.analysis.heap.objects.push(Object {
            ty,
            contents,
            open,
            containers: BTreeSet::new(),
        });
        self.work.objects.push(false);
        for slot in slots {
            if payload(ctx, self.analysis.heap.cells[slot].ty) == ty {
                self.add_layout_object(slot, id);
            }
        }
        if open {
            // Reserve the identity before the queued work follows recursive schema edges.
            self.analysis.heap.summaries.insert(ty, id);
            self.work.enqueue(Work::OpenObject(id));
        }
        id
    }

    fn heap_summary(&mut self, ctx: &Context, ty: TypeHandle) -> usize {
        let ty = payload(ctx, ty);
        self.analysis
            .heap
            .summaries
            .get(&ty)
            .copied()
            .unwrap_or_else(|| self.heap_object(ctx, ty, true))
    }

    fn heap_allocation(&mut self, ctx: &Context, id: usize, value: Value) -> Option<usize> {
        let cell = self.heap_value(ctx, id, value)?;
        Some(
            self.analysis.heap.cells[cell]
                .objects
                .first()
                .copied()
                .unwrap_or_else(|| {
                    let object = self.heap_object(ctx, value.get_type(ctx), false);
                    self.add_object(ctx, cell, object);
                    object
                }),
        )
    }

    fn heap_changed(&mut self, cell: usize) {
        for &reader in &self.analysis.heap.cells[cell].readers {
            self.work.enqueue(reader);
        }
    }

    /// The object after `previous` that `cell` may hold; iterating by position lets the
    /// visitor grow the set.
    fn next_object(&self, cell: usize, previous: Option<usize>) -> Option<usize> {
        after(&self.analysis.heap.cells[cell].objects, previous)
    }

    /// As [Self::next_object], over the objects whose layout `cell` shares.
    fn next_layout_object(&self, cell: usize, previous: Option<usize>) -> Option<usize> {
        after(&self.analysis.heap.cells[cell].layout_objects, previous)
    }

    /// Opens `object` once: its slots become unknown and every cell holding it is reconciled.
    fn mark_open(&mut self, object: usize) {
        if !self.analysis.heap.objects[object].open {
            self.analysis.heap.objects[object].open = true;
            self.work.enqueue(Work::OpenObject(object));
            self.reconcile_containers(object);
        }
    }

    /// Queues reconciliation of every cell whose layout may include `object`.
    fn reconcile_containers(&mut self, object: usize) {
        for &container in &self.analysis.heap.objects[object].containers {
            self.work.enqueue(Work::ReconcileCell(container));
        }
    }

    /// Queues reconciliation of the cells holding the object that owns slot `cell`.
    fn reconcile_owner(&mut self, cell: usize) {
        if let Some(owner) = self.analysis.heap.cells[cell].owner {
            self.reconcile_containers(owner);
        }
    }

    fn open_aliases(&mut self, cell: usize, reader: Work) {
        self.analysis.heap.cells[cell].readers.insert(reader);
        let mut previous = None;
        while let Some(object) = self.next_object(cell, previous) {
            previous = Some(object);
            self.mark_open(object);
        }
    }

    fn add_layout_object(&mut self, cell: usize, object: usize) {
        if self.analysis.heap.cells[cell].layout_objects.insert(object) {
            self.analysis.heap.objects[object].containers.insert(cell);
            self.work.enqueue(Work::ReconcileCell(cell));
            self.reconcile_owner(cell);
            self.heap_changed(cell);
        }
    }

    fn add_object(&mut self, ctx: &Context, cell: usize, object: usize) {
        self.add_layout_object(cell, object);
        if self.analysis.heap.cells[cell].objects.contains(&object) {
            return;
        }
        if self.analysis.heap.cells[cell].summarized {
            self.mark_open(object);
            return;
        }
        self.analysis.heap.cells[cell].objects.insert(object);
        if self.analysis.heap.cells[cell].objects.len() > super::max_origins() {
            // Existing aliases must also lose precision, including newly arriving objects.
            let ty = self.analysis.heap.cells[cell].ty;
            let summary = self.heap_summary(ctx, ty);
            self.open_aliases(cell, Work::OpenObject(summary));
            let targets = &mut self.analysis.heap.cells[cell];
            targets.objects.clear();
            targets.objects.insert(summary);
            targets.layout_objects.insert(summary);
            targets.summarized = true;
            self.analysis.heap.objects[summary].containers.insert(cell);
            self.analysis.summarized_heap_sets += 1;
            self.work.enqueue(Work::ReconcileCell(cell));
            self.reconcile_owner(cell);
        }
        self.heap_changed(cell);
    }

    fn copy_aliases(&mut self, ctx: &Context, from: usize, to: usize, reader: Work) {
        if from == to {
            return;
        }
        self.analysis.heap.cells[from].readers.insert(reader);
        let mut previous = None;
        while let Some(object) = self.next_object(from, previous) {
            previous = Some(object);
            self.add_object(ctx, to, object);
        }
        let mut previous = None;
        while let Some(object) = self.next_layout_object(from, previous) {
            previous = Some(object);
            self.add_layout_object(to, object);
        }
    }

    pub(super) fn heap_alias(
        &mut self,
        ctx: &Context,
        from_id: usize,
        from: Value,
        to_id: usize,
        to: Value,
        reader: Work,
    ) {
        if let (Some(from), Some(to)) = (
            self.heap_value(ctx, from_id, from),
            self.heap_value(ctx, to_id, to),
        ) {
            self.copy_aliases(ctx, from, to, reader);
        }
    }

    pub(super) fn seed_heap_parameters(&mut self, ctx: &Context, id: usize) {
        let body = &self.analysis.bodies[&self.analysis.keys[id].function];
        if !body.external {
            return;
        }
        for position in 0..body.parameters.len() {
            let parameter =
                self.analysis.bodies[&self.analysis.keys[id].function].parameters[position];
            if let Some(cell) = self.heap_value(ctx, id, parameter) {
                let object = self.heap_summary(ctx, parameter.get_type(ctx));
                self.add_object(ctx, cell, object);
            }
        }
    }

    pub(super) fn open_object(&mut self, ctx: &Context, object: usize) {
        self.widen_length(object, Range::of(Some(Integer::U32)));
        for position in 0..self.analysis.heap.objects[object].contents.slots().len() {
            let cell = self.analysis.heap.objects[object].contents.slots()[position];
            let ty = self.analysis.heap.cells[cell].ty;
            let integer = Integer::from_type(ctx, payload(ctx, ty));
            self.slot_domain(cell, integer.map_or(Fact::Original, Fact::Integer));
            let range = integer.map_or(Range::Unknown, |integer| Range::Bounds(integer.bounds()));
            self.slot_range(ctx, cell, range);
            self.slot_outcome(cell, Outcomes::unknown(ctx, ty));
            if aggregate(ctx, ty) {
                self.open_aliases(cell, Work::OpenObject(object));
                let summary = self.heap_summary(ctx, ty);
                self.add_object(ctx, cell, summary);
            }
        }
    }

    fn widen_length(&mut self, object: usize, range: Range) {
        let Contents::Array {
            length, readers, ..
        } = &mut self.analysis.heap.objects[object].contents
        else {
            return;
        };
        let joined = length.join(range, Some(Integer::U32));
        if joined != *length {
            *length = joined;
            for &reader in readers.iter() {
                self.work.enqueue(reader);
            }
        }
    }

    fn grow_array(&mut self, object: usize, index: Option<Range>, reader: Work) {
        let Contents::Array {
            length, readers, ..
        } = &mut self.analysis.heap.objects[object].contents
        else {
            return;
        };
        readers.insert(reader);
        if index == Some(Range::Bottom) {
            return;
        }
        if let Range::Bounds(length) = *length {
            if matches!(index, Some(Range::Bounds(index)) if index.upper < length.lower) {
                return;
            }
            self.widen_length(
                object,
                Range::Bounds(Interval {
                    lower: length.lower,
                    upper: u32::MAX.into(),
                }),
            );
        }
    }

    fn slot_domain(&mut self, slot: usize, domain: Fact) {
        let cell = &mut self.analysis.heap.cells[slot];
        let joined = cell.domain.join(domain);
        if joined != cell.domain {
            cell.domain = joined;
            self.heap_changed(slot);
            // Containers sharing a layout must agree once any of them holds a lossless 64-bit slot.
            if joined.needs_lossless_64() {
                self.reconcile_owner(slot);
            }
        }
    }

    fn slot_range(&mut self, ctx: &Context, slot: usize, range: Range) {
        let cell = &mut self.analysis.heap.cells[slot];
        let joined = cell.range.join(
            range,
            Integer::from_type(ctx, payload(ctx, cell.ty)).or(cell.domain.integer()),
        );
        if joined != cell.range {
            cell.range = joined;
            self.heap_changed(slot);
        }
    }

    fn slot_outcome(&mut self, slot: usize, outcome: Outcomes) {
        let cell = &mut self.analysis.heap.cells[slot];
        let joined = cell.outcome.join(outcome);
        if joined != cell.outcome {
            cell.outcome = joined;
            self.heap_changed(slot);
        }
    }

    fn write_slot(&mut self, ctx: &Context, id: usize, slot: usize, value: Value, reader: Work) {
        let domain = self.analysis.states[id].fact(value);
        let range = self.analysis.states[id].range(value);
        self.slot_domain(slot, domain);
        self.slot_range(ctx, slot, range);
        self.slot_outcome(slot, self.analysis.states[id].outcome(value));
        if let Some(from) = self.heap_value(ctx, id, value) {
            self.copy_aliases(ctx, from, slot, reader);
        }
    }

    /// Unifies the layouts of the objects a cell may hold: once one is open every one opens,
    /// numeric slots widen to agree, and nested layouts merge slot by slot.
    pub(super) fn reconcile_cell(&mut self, ctx: &Context, cell: usize) {
        let summarized = self.analysis.heap.cells[cell].summarized;
        // Copied: reconciling slots may insert into this cell's own layout through recursion.
        let objects: Vec<usize> = self.analysis.heap.cells[cell]
            .layout_objects
            .iter()
            .copied()
            .collect();
        if objects.is_empty() {
            return;
        }
        if summarized
            || objects
                .iter()
                .any(|&obj| self.analysis.heap.objects[obj].open)
        {
            for &obj in &objects {
                self.mark_open(obj);
            }
        }
        // The objects and, transitively, the owners of every cell that may hold one of them.
        let heap = &self.analysis.heap;
        let roots = objects.iter().copied().chain(heap.cells[cell].owner);
        let ancestors: BTreeSet<usize> = crate::reachable(roots, |object| {
            heap.objects[object]
                .containers
                .iter()
                .filter_map(|&container| heap.cells[container].owner)
        })
        .into_iter()
        .collect();
        // Nested slots of a recursive layout may hold their ancestors: link them, then descend.
        let mut visited: BTreeSet<usize> = objects.iter().copied().collect();
        let mut pending = objects.clone();
        while let Some(object) = pending.pop() {
            // Copied: linking a slot mutates the heap.
            let slots = self.analysis.heap.objects[object].contents.slots().to_vec();
            for slot in slots {
                let slot_ty = payload(ctx, self.analysis.heap.cells[slot].ty);
                for &ancestor in &ancestors {
                    if self.analysis.heap.objects[ancestor].ty == slot_ty {
                        self.add_layout_object(slot, ancestor);
                    }
                }
                let nested = &self.analysis.heap.cells[slot].layout_objects;
                pending.extend(nested.iter().filter(|&&nested| visited.insert(nested)));
            }
        }
        if objects.len() <= 1 {
            return;
        }
        let slot_count = self.analysis.heap.objects[objects[0]]
            .contents
            .slots()
            .len();
        for index in 0..slot_count {
            let slots: Vec<usize> = objects
                .iter()
                .filter_map(|&object| {
                    let contents = &self.analysis.heap.objects[object].contents;
                    contents.slots().get(index).copied()
                })
                .collect();
            self.reconcile_slot_group(ctx, &slots);
        }
    }

    pub(super) fn reconcile_slot_group(&mut self, ctx: &Context, slots: &[usize]) {
        let cells = &self.analysis.heap.cells;
        let lossless = |slot: usize| cells[slot].domain.needs_lossless_64();
        let narrow: Vec<usize> = if slots.iter().any(|&slot| lossless(slot)) {
            slots
                .iter()
                .copied()
                .filter(|&slot| !lossless(slot))
                .collect()
        } else {
            Vec::new()
        };
        let summarized = slots.iter().any(|&slot| cells[slot].summarized);
        let children: BTreeSet<usize> = slots
            .iter()
            .flat_map(|&slot| cells[slot].layout_objects.iter().copied())
            .collect();
        // Once any slot of the group is lossless, every slot widens to agree.
        for slot in narrow {
            self.slot_domain(slot, Fact::Numeric);
            self.slot_range(ctx, slot, Range::Unknown);
        }
        if summarized {
            for &child in &children {
                self.mark_open(child);
            }
        }
        for &slot in slots {
            for &child in &children {
                self.add_layout_object(slot, child);
            }
        }
    }
}
