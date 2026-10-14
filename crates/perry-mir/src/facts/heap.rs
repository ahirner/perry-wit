//! Allocation identities and stable slot facts, independent of physical storage.
//! Writes join into slots; readers subscribe to slots and possible object identities.
use super::*;
use crate::{
    collections::{
        ArrayAtOp, ArrayExtendOp, ArrayGetOp, ArrayLenOp, ArrayNewOp, ArrayPushOp, ArraySetOp,
    },
    heap::{AllocOp, FieldGetOp, FieldSetOp},
    intrinsics::{Combinator, PromiseCombinatorOp},
    optional::{OptionalCastOp, UnwrapOp, WrapOp},
    types::{ArrayType, RecordType},
};
use std::ops::Bound::{Excluded, Unbounded};

const MAX_ORIGINS: usize = 32;

struct Cell {
    ty: TypeHandle,
    objects: BTreeSet<usize>,
    summarized: bool,
    domain: Fact,
    range: Range,
    outcome: Outcomes,
    readers: BTreeSet<Work>,
}

struct Object {
    contents: Contents,
    open: bool,
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

#[derive(Clone, Copy)]
pub(super) enum OperationKind {
    Allocate { array: bool },
    Read { slot: usize },
    Write { slot: usize, value: usize },
    ArrayWrite { index: Option<usize>, value: usize },
    Length,
    Combine(Combinator),
    Extend,
    Alias { operand: usize },
    Return,
    Call,
    Unknown,
    None,
}

impl OperationKind {
    pub(super) fn classify(ctx: &Context, op: Ptr<Operation>) -> Self {
        if Operation::get_op::<crate::optional::UndefinedOp>(op, ctx).is_some()
            || Operation::get_op::<crate::optional::NullOp>(op, ctx).is_some()
        {
            return Self::None;
        }
        if Operation::get_op::<WrapOp>(op, ctx).is_some()
            || Operation::get_op::<UnwrapOp>(op, ctx).is_some()
            || Operation::get_op::<OptionalCastOp>(op, ctx).is_some()
            || Operation::get_op::<AwaitOp>(op, ctx).is_some()
        {
            return Self::Alias { operand: 0 };
        }
        if let Some(operand) = promise_payload(ctx, op) {
            return Self::Alias { operand };
        }
        let operation = op.deref(ctx);
        if !operation
            .operands()
            .chain(operation.results())
            .any(|value| aggregate(ctx, value.get_type(ctx)))
        {
            return Self::None;
        }
        if Operation::get_op::<AllocOp>(op, ctx).is_some() {
            return Self::Allocate { array: false };
        }
        if Operation::get_op::<ArrayNewOp>(op, ctx).is_some() {
            return Self::Allocate { array: true };
        }
        if let Some(combine) = Operation::get_op::<PromiseCombinatorOp>(op, ctx) {
            return Self::Combine(combine.combinator(ctx));
        }
        let field = Operation::get_op::<FieldGetOp>(op, ctx)
            .map(|op| (op.field(ctx), true))
            .or_else(|| Operation::get_op::<FieldSetOp>(op, ctx).map(|op| (op.field(ctx), false)));
        if let Some((field, read)) = field {
            let ty = operation.get_operand(0).get_type(ctx).deref(ctx);
            let Some(slot) = ty
                .downcast_ref::<RecordType>()
                .and_then(|rec| rec.fields().ok())
                .and_then(|fields| fields.iter().position(|(name, _)| *name == field))
            else {
                return Self::Unknown;
            };
            return if read {
                Self::Read { slot }
            } else {
                Self::Write { slot, value: 1 }
            };
        }
        if Operation::get_op::<ArrayGetOp>(op, ctx).is_some()
            || Operation::get_op::<ArrayAtOp>(op, ctx).is_some()
        {
            return Self::Read { slot: 0 };
        }
        if Operation::get_op::<ArraySetOp>(op, ctx).is_some() {
            return Self::ArrayWrite {
                index: Some(1),
                value: 2,
            };
        }
        if Operation::get_op::<ArrayPushOp>(op, ctx).is_some() {
            return Self::ArrayWrite {
                index: None,
                value: 1,
            };
        }
        if Operation::get_op::<ArrayLenOp>(op, ctx).is_some() {
            return Self::Length;
        }
        if Operation::get_op::<ArrayExtendOp>(op, ctx).is_some() {
            return Self::Extend;
        }
        if Operation::get_op::<ReturnOp>(op, ctx).is_some() {
            return Self::Return;
        }
        if Operation::get_op::<CallOp>(op, ctx).is_some()
            || Operation::get_op::<AsyncCallOp>(op, ctx).is_some()
        {
            return Self::Call;
        }
        if op_cast::<dyn BranchOpInterface>(&*Operation::get_op_dyn(op, ctx)).is_some()
            || Operation::get_op::<crate::optional::IsUndefinedOp>(op, ctx).is_some()
            || Operation::get_op::<crate::optional::IsNullOp>(op, ctx).is_some()
        {
            return Self::None;
        }
        Self::Unknown
    }
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

fn aggregate(ctx: &Context, ty: TypeHandle) -> bool {
    let ty = payload(ctx, ty).deref(ctx);
    ty.is::<RecordType>() || ty.is::<ArrayType>()
}

impl Solver {
    fn heap_cell(&mut self, ty: TypeHandle) -> usize {
        let id = self.analysis.heap.cells.len();
        self.analysis.heap.cells.push(Cell {
            ty,
            objects: BTreeSet::new(),
            summarized: false,
            domain: Fact::Bottom,
            range: Range::Bottom,
            outcome: Outcomes::Bottom,
            readers: BTreeSet::new(),
        });
        id
    }

    fn heap_value(&mut self, ctx: &Context, id: usize, value: Value) -> Option<usize> {
        if !aggregate(ctx, value.get_type(ctx)) {
            return None;
        }
        if let Some(&cell) = self.analysis.states[id].heap_values.get(&value) {
            return Some(cell);
        }
        let cell = self.heap_cell(value.get_type(ctx));
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
                    .map(|&(_, ty)| self.heap_cell(ty))
                    .collect(),
            )
        } else {
            let array = shape.downcast_ref::<ArrayType>().expect("aggregate schema");
            Contents::Array {
                element: self.heap_cell(array.element()),
                length: Range::Bottom,
                readers: BTreeSet::new(),
            }
        };
        self.analysis.heap.objects.push(Object { contents, open });
        self.work.objects.push(false);
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

    fn next_object(&self, cell: usize, previous: Option<usize>) -> Option<usize> {
        self.analysis.heap.cells[cell]
            .objects
            .range((previous.map_or(Unbounded, Excluded), Unbounded))
            .next()
            .copied()
    }

    fn open_aliases(&mut self, cell: usize, reader: Work) {
        self.analysis.heap.cells[cell].readers.insert(reader);
        let mut previous = None;
        while let Some(object) = self.next_object(cell, previous) {
            previous = Some(object);
            if !self.analysis.heap.objects[object].open {
                self.analysis.heap.objects[object].open = true;
                self.work.enqueue(Work::OpenObject(object));
            }
        }
    }

    fn add_object(&mut self, ctx: &Context, cell: usize, object: usize) {
        if self.analysis.heap.cells[cell].objects.contains(&object) {
            return;
        }
        if self.analysis.heap.cells[cell].summarized {
            if !self.analysis.heap.objects[object].open {
                self.analysis.heap.objects[object].open = true;
                self.work.enqueue(Work::OpenObject(object));
            }
            return;
        }
        self.analysis.heap.cells[cell].objects.insert(object);
        if self.analysis.heap.cells[cell].objects.len() > MAX_ORIGINS {
            // Existing aliases must also lose precision, including newly arriving objects.
            let ty = self.analysis.heap.cells[cell].ty;
            let summary = self.heap_summary(ctx, ty);
            self.open_aliases(cell, Work::OpenObject(summary));
            let targets = &mut self.analysis.heap.cells[cell];
            targets.objects.clear();
            targets.objects.insert(summary);
            targets.summarized = true;
            self.analysis.summarized_heap_sets += 1;
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
        self.widen_length(
            object,
            Range::Bounds(
                Integer {
                    bits: 32,
                    signed: false,
                }
                .bounds(),
            ),
        );
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
        let joined = length.join(
            range,
            Some(Integer {
                bits: 32,
                signed: false,
            }),
        );
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
        self.slot_domain(slot, self.analysis.states[id].fact(value));
        self.slot_range(ctx, slot, self.analysis.states[id].range(value));
        self.slot_outcome(slot, self.analysis.states[id].outcome(value));
        if let Some(from) = self.heap_value(ctx, id, value) {
            self.copy_aliases(ctx, from, slot, reader);
        }
    }

    pub(super) fn heap_call(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        child: usize,
        op: Ptr<Operation>,
    ) {
        let reader = Work::Operation(id, index);
        for (position, input) in op.deref(ctx).operands().enumerate() {
            let parameter =
                self.analysis.bodies[&self.analysis.keys[child].function].parameters[position];
            self.range(child, parameter, self.analysis.states[id].range(input));
            self.outcome(child, parameter, self.analysis.states[id].outcome(input));
            self.heap_alias(ctx, id, input, child, parameter, reader);
        }
        for (position, result) in op.deref(ctx).results().enumerate() {
            if let Some(to) = self.heap_value(ctx, id, result)
                && let Some(from) = self.analysis.states[child].results[position].heap
            {
                self.copy_aliases(ctx, from, to, reader);
            }
        }
    }

    pub(super) fn heap_operation(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        op: Ptr<Operation>,
        kind: OperationKind,
    ) -> Option<(Fact, Range)> {
        let reader = Work::Operation(id, index);
        let operation = op.deref(ctx);
        match kind {
            OperationKind::Allocate { array } => {
                let value = operation.get_result(0);
                let object = self.heap_allocation(ctx, id, value)?;
                if array {
                    let length = operation.get_num_operands() as i128;
                    self.widen_length(
                        object,
                        Range::Bounds(Interval {
                            lower: length,
                            upper: length,
                        }),
                    );
                }
                for (position, value) in operation.operands().enumerate() {
                    let slot = self.analysis.heap.objects[object].contents.slots()
                        [if array { 0 } else { position }];
                    self.write_slot(ctx, id, slot, value, reader);
                }
            }
            OperationKind::Read { .. }
            | OperationKind::Write { .. }
            | OperationKind::ArrayWrite { .. } => {
                let position = match kind {
                    OperationKind::Read { slot } | OperationKind::Write { slot, .. } => slot,
                    _ => 0,
                };
                let base = self.heap_value(ctx, id, operation.get_operand(0))?;
                self.analysis.heap.cells[base].readers.insert(reader);
                let result = if matches!(kind, OperationKind::Read { .. }) {
                    self.heap_value(ctx, id, operation.get_result(0))
                } else {
                    None
                };
                let mut domain = Fact::Bottom;
                let mut range = Range::Bottom;
                let mut outcome = Outcomes::Bottom;
                let mut previous = None;
                while let Some(object) = self.next_object(base, previous) {
                    previous = Some(object);
                    let slot = self.analysis.heap.objects[object].contents.slots()[position];
                    if let OperationKind::ArrayWrite { index, .. } = kind {
                        let range = index.map(|index| {
                            self.analysis.states[id].range(operation.get_operand(index))
                        });
                        self.grow_array(object, range, reader);
                    }
                    if let OperationKind::Write { value, .. }
                    | OperationKind::ArrayWrite { value, .. } = kind
                    {
                        self.write_slot(ctx, id, slot, operation.get_operand(value), reader);
                    } else {
                        self.analysis.heap.cells[slot].readers.insert(reader);
                        domain = domain.join(self.analysis.heap.cells[slot].domain);
                        range = range.join(
                            self.analysis.heap.cells[slot].range,
                            Integer::from_type(
                                ctx,
                                payload(ctx, self.analysis.heap.cells[slot].ty),
                            )
                            .or(domain.integer()),
                        );
                        outcome = outcome.join(self.analysis.heap.cells[slot].outcome);
                        if let Some(result) = result {
                            self.copy_aliases(ctx, slot, result, reader);
                        }
                    }
                }
                if matches!(kind, OperationKind::Read { .. }) {
                    self.outcome(id, operation.get_result(0), outcome);
                }
                return matches!(kind, OperationKind::Read { .. }).then_some((domain, range));
            }
            OperationKind::Extend => {
                let to = self.heap_value(ctx, id, operation.get_operand(0))?;
                let from = self.heap_value(ctx, id, operation.get_operand(1))?;
                self.analysis.heap.cells[to].readers.insert(reader);
                self.analysis.heap.cells[from].readers.insert(reader);
                let mut source = None;
                while let Some(object) = self.next_object(from, source) {
                    source = Some(object);
                    let input = self.analysis.heap.objects[object].contents.slots()[0];
                    self.analysis.heap.cells[input].readers.insert(reader);
                    let mut target = None;
                    while let Some(object) = self.next_object(to, target) {
                        target = Some(object);
                        self.grow_array(object, None, reader);
                        let output = self.analysis.heap.objects[object].contents.slots()[0];
                        self.slot_domain(output, self.analysis.heap.cells[input].domain);
                        self.slot_range(ctx, output, self.analysis.heap.cells[input].range);
                        self.slot_outcome(output, self.analysis.heap.cells[input].outcome);
                        self.copy_aliases(ctx, input, output, reader);
                    }
                }
            }
            OperationKind::Length => {
                let base = self.heap_value(ctx, id, operation.get_operand(0))?;
                self.analysis.heap.cells[base].readers.insert(reader);
                let mut previous = None;
                while let Some(object) = self.next_object(base, previous) {
                    previous = Some(object);
                    if let Contents::Array { readers, .. } =
                        &mut self.analysis.heap.objects[object].contents
                    {
                        readers.insert(reader);
                    }
                }
                return Some((Fact::Original, self.analysis.heap.length(base)));
            }
            OperationKind::Combine(kind) => {
                let input = self.heap_value(ctx, id, operation.get_operand(0))?;
                self.analysis.heap.cells[input].readers.insert(reader);
                let result = operation.get_result(0);
                let output = if kind == Combinator::Race {
                    None
                } else {
                    self.heap_allocation(ctx, id, result)
                };
                let mut outcome = if kind == Combinator::Race {
                    Outcomes::Bottom
                } else {
                    Outcomes::Fulfilled
                };
                let mut domain = Fact::Bottom;
                let mut range = Range::Bottom;
                let mut previous = None;
                while let Some(object) = self.next_object(input, previous) {
                    previous = Some(object);
                    let Contents::Array {
                        element,
                        length,
                        readers,
                    } = &mut self.analysis.heap.objects[object].contents
                    else {
                        continue;
                    };
                    readers.insert(reader);
                    let (element, length) = (*element, *length);
                    self.analysis.heap.cells[element].readers.insert(reader);
                    if kind != Combinator::AllSettled {
                        outcome = outcome.join(self.analysis.heap.cells[element].outcome);
                        if let Some(object) = output {
                            let slot = self.analysis.heap.objects[object].contents.slots()[0];
                            self.slot_domain(slot, self.analysis.heap.cells[element].domain);
                            self.slot_range(ctx, slot, self.analysis.heap.cells[element].range);
                            self.slot_outcome(
                                slot,
                                Outcomes::unknown(ctx, self.analysis.heap.cells[slot].ty),
                            );
                            self.copy_aliases(ctx, element, slot, reader);
                        } else {
                            domain = domain.join(self.analysis.heap.cells[element].domain);
                            range = range
                                .join(self.analysis.heap.cells[element].range, domain.integer());
                            if let Some(result) = self.heap_value(ctx, id, result) {
                                self.copy_aliases(ctx, element, result, reader);
                            }
                        }
                    }
                    if let Some(object) = output {
                        self.widen_length(object, length);
                    }
                }
                self.outcome(id, result, outcome);
                return (kind == Combinator::Race).then_some((domain, range));
            }
            OperationKind::Alias { operand } => {
                let input = operation.get_operand(operand);
                self.heap_alias(ctx, id, input, id, operation.get_result(0), reader);
                if promise_payload(ctx, op).is_none()
                    && Operation::get_op::<AwaitOp>(op, ctx).is_none()
                {
                    self.outcome(
                        id,
                        operation.get_result(0),
                        self.analysis.states[id].outcome(input),
                    );
                }
                return Some((
                    self.analysis.states[id].fact(input),
                    self.analysis.states[id].range(input),
                ));
            }
            OperationKind::Return => {
                for (position, value) in operation.operands().enumerate() {
                    if let Some(from) = self.heap_value(ctx, id, value) {
                        let to = if let Some(to) = self.analysis.states[id].results[position].heap {
                            to
                        } else {
                            let to = self.heap_cell(value.get_type(ctx));
                            self.analysis.states[id].results[position].heap = Some(to);
                            self.notify_callers(id);
                            to
                        };
                        self.copy_aliases(ctx, from, to, reader);
                    }
                }
            }
            OperationKind::Unknown | OperationKind::Call => {
                if matches!(kind, OperationKind::Call) {
                    let callee = Operation::get_op::<CallOp>(op, ctx)
                        .map(|call| call.callee(ctx))
                        .or_else(|| {
                            Operation::get_op::<AsyncCallOp>(op, ctx).map(|call| call.callee(ctx))
                        })?;
                    if self.analysis.bodies.contains_key(&callee) {
                        return None;
                    }
                }
                // Unmodelled storage may retain aliases or mutate reachable objects.
                for value in operation.operands() {
                    if let Some(cell) = self.heap_value(ctx, id, value) {
                        self.open_aliases(cell, reader);
                    }
                }
                for value in operation.results() {
                    if let Some(cell) = self.heap_value(ctx, id, value) {
                        let object = self.heap_summary(ctx, value.get_type(ctx));
                        self.add_object(ctx, cell, object);
                    }
                }
            }
            OperationKind::None => {}
        }
        None
    }
}
