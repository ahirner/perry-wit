//! Classification of MIR operations by their heap effect, and their transfer over `HeapFacts`.
use super::*;
use crate::{
    is_any,
    optional::{IsNullOp, IsUndefinedOp, NullOp, OmittedOp, ToNumberOp, UndefinedOp},
};

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
    JsonSpread,
    Alias { operand: usize },
    Return,
    Call,
    Unknown,
    None,
}

impl OperationKind {
    pub(in crate::facts) fn classify(ctx: &Context, op: Ptr<Operation>) -> Self {
        let operation = op.deref(ctx);
        let touches_aggregate = || {
            let mut values = operation.operands().chain(operation.results());
            values.any(|value| aggregate(ctx, value.get_type(ctx)))
        };
        let field = Operation::get_op::<FieldGetOp>(op, ctx)
            .map(|op| (op.field(ctx), true))
            .or_else(|| Operation::get_op::<FieldSetOp>(op, ctx).map(|op| (op.field(ctx), false)));
        if is_any::<(UndefinedOp, OmittedOp, NullOp, crate::heap::FieldPresentOp)>(ctx, op) {
            Self::None
        } else if is_any::<(WrapOp, UnwrapOp, OptionalCastOp, ToNumberOp, AwaitOp)>(ctx, op) {
            Self::Alias { operand: 0 }
        } else if let Some(operand) = promise_payload(ctx, op) {
            Self::Alias { operand }
        } else if !touches_aggregate() {
            Self::None
        } else if Operation::is_op::<AllocOp>(op, ctx) {
            Self::Allocate { array: false }
        } else if Operation::is_op::<ArrayNewOp>(op, ctx) {
            Self::Allocate { array: true }
        } else if let Some(combine) = Operation::get_op::<PromiseCombinatorOp>(op, ctx) {
            Self::Combine(combine.combinator(ctx))
        } else if let Some((field, read)) = field {
            let record = operation.get_operand(0).get_type(ctx);
            let record = record.deref(ctx);
            let slot = record
                .downcast_ref::<RecordType>()
                .and_then(|rec| rec.field(&field));
            match slot {
                Some((slot, _)) if read => Self::Read { slot },
                Some((slot, _)) => Self::Write { slot, value: 1 },
                None => Self::Unknown,
            }
        } else if is_any::<(ArrayGetOp, ArrayAtOp)>(ctx, op) {
            Self::Read { slot: 0 }
        } else if Operation::is_op::<ArraySetOp>(op, ctx) {
            Self::ArrayWrite {
                index: Some(1),
                value: 2,
            }
        } else if Operation::is_op::<ArrayPushOp>(op, ctx) {
            Self::ArrayWrite {
                index: None,
                value: 1,
            }
        } else if Operation::is_op::<ArrayLenOp>(op, ctx) {
            Self::Length
        } else if Operation::is_op::<ArrayExtendOp>(op, ctx) {
            Self::Extend
        } else if Operation::is_op::<ArrayTruncateOp>(op, ctx) {
            Self::Truncate
        } else if is_any::<(ConcatOp, ConcatReuseOp)>(ctx, op) {
            Self::Concat
        } else if Operation::is_op::<ConcatSegmentsOp>(op, ctx) {
            Self::ConcatSegments
        } else if Operation::is_op::<ReturnOp>(op, ctx) {
            Self::Return
        } else if is_any::<(CallOp, AsyncCallOp)>(ctx, op) {
            Self::Call
        } else if op_cast::<dyn BranchOpInterface>(&*Operation::get_op_dyn(op, ctx)).is_some()
            || is_any::<(IsUndefinedOp, IsNullOp, crate::intrinsics::JsonStringifyOp)>(ctx, op)
        {
            Self::None
        } else {
            match crate::intrinsics::intrinsic_of(ctx, op) {
                Some(crate::Intrinsic::JsonSpread | crate::Intrinsic::JsonOrderCopy) => {
                    Self::JsonSpread
                }
                Some(crate::Intrinsic::JsonExclude | crate::Intrinsic::JsonInsert) => Self::None,
                _ => Self::Unknown,
            }
        }
    }
}

impl Solver {
    /// Join slot `from`'s domain, range, settlement and aliases into slot `to`.
    fn copy_slot(&mut self, ctx: &Context, from: usize, to: usize, reader: Work) {
        let cell = &self.analysis.heap.cells[from];
        let (domain, range, outcome) = (cell.domain, cell.range, cell.outcome);
        self.slot_domain(to, domain);
        self.slot_range(ctx, to, range);
        self.slot_outcome(to, outcome);
        self.copy_aliases(ctx, from, to, reader);
    }

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
                let use_layout = self.analysis.heap.cells[base].objects.is_empty();
                let mut previous = None;
                while let Some(object) = if use_layout {
                    self.next_layout_object(base, previous)
                } else {
                    self.next_object(base, previous)
                } {
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
                        self.copy_slot(ctx, input, output, reader);
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
                        self.copy_slot(ctx, input, out_slot, reader);
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
                self.widen_length(output, Range::of(Some(Integer::U32)));
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
                            self.copy_slot(ctx, elem_slot, out_slot, reader);
                        }
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
                self.widen_length(output, Range::of(Some(Integer::U32)));
            }
            OperationKind::Truncate => {
                let base = self.heap_value(ctx, id, operation.get_operand(0))?;
                self.analysis.heap.cells[base].readers.insert(reader);
                let mut previous = None;
                while let Some(object) = self.next_object(base, previous) {
                    previous = Some(object);
                    self.widen_length(object, Range::of(Some(Integer::U32)));
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
                            let to = self.heap_cell(value.get_type(ctx), None);
                            self.analysis.states[id].results[position].heap = Some(to);
                            self.notify_callers(id);
                            to
                        };
                        self.copy_aliases(ctx, from, to, reader);
                    }
                }
            }
            OperationKind::JsonSpread => {
                if let (Some(to), Some(from)) = (
                    self.heap_value(ctx, id, operation.get_operand(0)),
                    self.heap_value(ctx, id, operation.get_operand(1)),
                ) {
                    self.analysis.heap.cells[to].readers.insert(reader);
                    self.analysis.heap.cells[from].readers.insert(reader);
                    let src_open = self.analysis.heap.cells[from].summarized
                        || self.analysis.heap.cells[from]
                            .objects
                            .iter()
                            .any(|&obj| self.analysis.heap.objects[obj].open);
                    if src_open {
                        self.open_aliases(to, reader);
                    }
                }
            }
            OperationKind::Unknown | OperationKind::Call => {
                if matches!(kind, OperationKind::Call) {
                    let callee = crate::calls::callee(ctx, op)?;
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
