//! Classification of MIR operations by their heap effect, and their transfer over `HeapFacts`.
use super::*;

#[derive(Clone, Copy)]
pub(in crate::facts) enum OperationKind {
    Allocate { array: bool },
    Read { slot: usize },
    Write { slot: usize, value: usize },
    ArrayWrite { index: Option<usize>, value: usize },
    Length,
    Combine(Combinator),
    Extend,
    Truncate,
    Concat,
    ConcatSegments,
    Alias { operand: usize },
    Return,
    Call,
    Unknown,
    None,
}

impl OperationKind {
    pub(in crate::facts) fn classify(ctx: &Context, op: Ptr<Operation>) -> Self {
        if Operation::get_op::<crate::optional::UndefinedOp>(op, ctx).is_some()
            || Operation::get_op::<crate::optional::OmittedOp>(op, ctx).is_some()
            || Operation::get_op::<crate::optional::NullOp>(op, ctx).is_some()
            || Operation::get_op::<crate::heap::FieldPresentOp>(op, ctx).is_some()
        {
            return Self::None;
        }
        if Operation::get_op::<WrapOp>(op, ctx).is_some()
            || Operation::get_op::<UnwrapOp>(op, ctx).is_some()
            || Operation::get_op::<OptionalCastOp>(op, ctx).is_some()
            || Operation::get_op::<crate::optional::ToNumberOp>(op, ctx).is_some()
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
        if Operation::get_op::<ArrayTruncateOp>(op, ctx).is_some() {
            return Self::Truncate;
        }
        if Operation::get_op::<ConcatOp>(op, ctx).is_some()
            || Operation::get_op::<ConcatReuseOp>(op, ctx).is_some()
        {
            return Self::Concat;
        }
        if Operation::get_op::<ConcatSegmentsOp>(op, ctx).is_some() {
            return Self::ConcatSegments;
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
            || Operation::get_op::<crate::intrinsics::JsonStringifyOp>(op, ctx).is_some()
        {
            return Self::None;
        }
        Self::Unknown
    }
}

impl Solver {
    pub(in crate::facts) fn heap_call(
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

    pub(in crate::facts) fn heap_operation(
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
                let mut slots = Vec::new();
                let mut source = None;
                while let Some(object) = self.next_object(from, source) {
                    source = Some(object);
                    let input = self.analysis.heap.objects[object].contents.slots()[0];
                    self.analysis.heap.cells[input].readers.insert(reader);
                    slots.push(input);
                    let mut target = None;
                    while let Some(object) = self.next_object(to, target) {
                        target = Some(object);
                        self.grow_array(object, None, reader);
                        let output = self.analysis.heap.objects[object].contents.slots()[0];
                        slots.push(output);
                        self.slot_domain(output, self.analysis.heap.cells[input].domain);
                        self.slot_range(ctx, output, self.analysis.heap.cells[input].range);
                        self.slot_outcome(output, self.analysis.heap.cells[input].outcome);
                        self.copy_aliases(ctx, input, output, reader);
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
            }
            OperationKind::Concat => {
                let result = operation.get_result(0);
                let output = self.heap_allocation(ctx, id, result)?;
                let out_slot = self.analysis.heap.objects[output].contents.slots()[0];
                let mut slots = vec![out_slot];
                for input_val in operation.operands() {
                    let from = match self.heap_value(ctx, id, input_val) {
                        Some(f) => f,
                        None => continue,
                    };
                    self.analysis.heap.cells[from].readers.insert(reader);
                    let mut source = None;
                    while let Some(object) = self.next_object(from, source) {
                        source = Some(object);
                        let input = self.analysis.heap.objects[object].contents.slots()[0];
                        self.analysis.heap.cells[input].readers.insert(reader);
                        slots.push(input);
                        self.slot_domain(out_slot, self.analysis.heap.cells[input].domain);
                        self.slot_range(ctx, out_slot, self.analysis.heap.cells[input].range);
                        self.slot_outcome(out_slot, self.analysis.heap.cells[input].outcome);
                        self.copy_aliases(ctx, input, out_slot, reader);
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
                self.widen_length(
                    output,
                    Range::Bounds(Interval {
                        lower: 0,
                        upper: u32::MAX.into(),
                    }),
                );
            }
            OperationKind::ConcatSegments => {
                let result = operation.get_result(0);
                let output = self.heap_allocation(ctx, id, result)?;
                let out_slot = self.analysis.heap.objects[output].contents.slots()[0];
                let segments_val = operation.get_operand(0);
                let mut slots = vec![out_slot];
                if let Some(segments_cell) = self.heap_value(ctx, id, segments_val) {
                    self.analysis.heap.cells[segments_cell]
                        .readers
                        .insert(reader);
                    let mut seg_obj = None;
                    while let Some(object) = self.next_object(segments_cell, seg_obj) {
                        seg_obj = Some(object);
                        let seg_slot = self.analysis.heap.objects[object].contents.slots()[0];
                        self.analysis.heap.cells[seg_slot].readers.insert(reader);
                        let mut inner_obj = None;
                        while let Some(inner) = self.next_object(seg_slot, inner_obj) {
                            inner_obj = Some(inner);
                            let elem_slot = self.analysis.heap.objects[inner].contents.slots()[0];
                            self.analysis.heap.cells[elem_slot].readers.insert(reader);
                            slots.push(elem_slot);
                            self.slot_domain(out_slot, self.analysis.heap.cells[elem_slot].domain);
                            self.slot_range(
                                ctx,
                                out_slot,
                                self.analysis.heap.cells[elem_slot].range,
                            );
                            self.slot_outcome(
                                out_slot,
                                self.analysis.heap.cells[elem_slot].outcome,
                            );
                            self.copy_aliases(ctx, elem_slot, out_slot, reader);
                        }
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
                self.widen_length(
                    output,
                    Range::Bounds(Interval {
                        lower: 0,
                        upper: u32::MAX.into(),
                    }),
                );
            }
            OperationKind::Truncate => {
                let base = self.heap_value(ctx, id, operation.get_operand(0))?;
                self.analysis.heap.cells[base].readers.insert(reader);
                let mut previous = None;
                while let Some(object) = self.next_object(base, previous) {
                    previous = Some(object);
                    self.widen_length(
                        object,
                        Range::Bounds(Interval {
                            lower: 0,
                            upper: u32::MAX.into(),
                        }),
                    );
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
                let is_external = self.analysis.bodies[&self.analysis.keys[id].function].external;
                for (position, value) in operation.operands().enumerate() {
                    if let Some(from) = self.heap_value(ctx, id, value) {
                        if is_external {
                            self.open_aliases(from, reader);
                        }
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
