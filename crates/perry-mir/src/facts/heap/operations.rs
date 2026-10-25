//! Classification of MIR operations by their heap effect, and their transfer over `HeapFacts`.
use super::*;
use crate::{effects::Mutation, intrinsics::Intrinsic, semantics::OperationKind};

impl Solver {
    fn array_elements(&mut self, cell: usize, depth: u8, reader: Work, slots: &mut Vec<usize>) {
        self.analysis.heap.cells[cell].readers.insert(reader);
        let mut previous = None;
        while let Some(object) = self.next_object(cell, previous) {
            previous = Some(object);
            let slot = self.analysis.heap.objects[object].contents.slots()[0];
            if depth > 1 {
                self.array_elements(slot, depth - 1, reader, slots);
            } else {
                self.analysis.heap.cells[slot].readers.insert(reader);
                slots.push(slot);
            }
        }
    }

    /// Join slot `from`'s domain, range, settlement and aliases into slot `to`.
    fn copy_slot(&mut self, ctx: &Context, from: usize, to: usize, reader: Work) -> Result<()> {
        self.update_slot(ctx, to, self.analysis.heap.cells[from].facts);
        self.copy_aliases(ctx, from, to, reader)
    }

    pub(in crate::facts) fn heap_call(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        child: usize,
        op: Ptr<Operation>,
    ) -> Result<()> {
        let reader = Work::Operation(id, index);
        for (position, input) in op.deref(ctx).operands().enumerate() {
            let parameter =
                self.analysis.bodies[&self.analysis.keys[child].function].parameters[position];
            let mut facts = self.analysis.states[id].value(input);
            facts.domain = Fact::Bottom;
            self.update(child, parameter, facts);
            self.heap_alias(ctx, id, input, child, parameter, reader)?;
        }
        for (position, result) in op.deref(ctx).results().enumerate() {
            if let Some(to) = self.heap_value(ctx, id, result)
                && let Some(from) = self.analysis.states[child].results[position].heap
            {
                self.copy_aliases(ctx, from, to, reader)?;
            }
        }
        Ok(())
    }

    pub(in crate::facts) fn heap_operation(
        &mut self,
        ctx: &Context,
        id: usize,
        index: usize,
        op: Ptr<Operation>,
        kind: OperationKind,
    ) -> Result<Option<ValueFacts>> {
        let reader = Work::Operation(id, index);
        let operation = op.deref(ctx);
        if matches!(
            kind,
            OperationKind::Alias
                | OperationKind::Await
                | OperationKind::Resolved
                | OperationKind::Timer
        ) {
            let input = operation.get_operand(kind.promise_payload().unwrap_or(0));
            self.heap_alias(ctx, id, input, id, operation.get_result(0), reader)?;
            return Ok(Some(self.analysis.states[id].value(input)));
        }
        if let OperationKind::Call { .. } = kind
            && crate::calls::callee(ctx, op)
                .is_some_and(|callee| self.analysis.bodies.contains_key(&callee))
        {
            return Ok(None);
        }
        if !operation
            .operands()
            .chain(operation.results())
            .any(|value| aggregate(ctx, value.get_type(ctx)))
        {
            return Ok(None);
        }
        if Operation::is_op::<crate::JsonCopyOp>(op, ctx) {
            // Decoding creates fresh storage with the declared JSON numeric contract.
            // Ordinary numbers may round; fixed-width integer targets remain exact.
            // The source is read-only and must retain its independently inferred layout.
            let value = operation.get_result(0);
            if let Some(cell) = self.heap_value(ctx, id, value) {
                let object = self.heap_summary(ctx, value.get_type(ctx))?;
                self.add_object(ctx, cell, object)?;
            }
            return Ok(None);
        }
        match kind {
            OperationKind::Allocate { array } => {
                let value = operation.get_result(0);
                let Some(object) = self.heap_allocation(ctx, id, value)? else {
                    return Ok(None);
                };
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
                    self.write_slot(ctx, id, slot, value, reader)?;
                }
            }
            OperationKind::Read { .. }
            | OperationKind::Mutate(Mutation::Field { slot: Some(_) } | Mutation::Element { .. }) =>
            {
                let position = match kind {
                    OperationKind::Read { slot }
                    | OperationKind::Mutate(Mutation::Field { slot: Some(slot) }) => slot,
                    _ => 0,
                };
                let Some(base) = self.heap_value(ctx, id, operation.get_operand(0)) else {
                    return Ok(None);
                };
                self.analysis.heap.cells[base].readers.insert(reader);
                let result = if matches!(kind, OperationKind::Read { .. }) {
                    self.heap_value(ctx, id, operation.get_result(0))
                } else {
                    None
                };
                let mut facts = ValueFacts::default();
                let use_layout = self.analysis.heap.cells[base].objects.is_empty();
                let mut previous = None;
                while let Some(object) = if use_layout {
                    self.next_layout_object(base, previous)
                } else {
                    self.next_object(base, previous)
                } {
                    previous = Some(object);
                    let slot = self.analysis.heap.objects[object].contents.slots()[position];
                    if let OperationKind::Mutate(Mutation::Element { index }) = kind {
                        let range = index.map(|index| {
                            self.analysis.states[id].range(operation.get_operand(index))
                        });
                        self.grow_array(object, range, reader);
                    }
                    if let OperationKind::Mutate(mutation) = kind
                        && let Some(value) = mutation.stored_operand()
                    {
                        self.write_slot(ctx, id, slot, operation.get_operand(value), reader)?;
                    } else {
                        self.analysis.heap.cells[slot].readers.insert(reader);
                        let cell = &self.analysis.heap.cells[slot];
                        facts.join(
                            cell.facts,
                            Representation::Storage(Integer::from_type(ctx, payload(ctx, cell.ty))),
                        );
                        if let Some(result) = result {
                            self.copy_aliases(ctx, slot, result, reader)?;
                        }
                    }
                }
                return Ok(matches!(kind, OperationKind::Read { .. }).then_some(facts));
            }
            OperationKind::Mutate(Mutation::Extend) | OperationKind::Concat { .. } => {
                let extends = matches!(kind, OperationKind::Mutate(Mutation::Extend));
                let target = if extends {
                    operation.get_operand(0)
                } else {
                    operation.get_result(0)
                };
                if !extends {
                    self.heap_allocation(ctx, id, target)?;
                }
                let Some(target) = self.heap_value(ctx, id, target) else {
                    return Ok(None);
                };
                self.analysis.heap.cells[target].readers.insert(reader);
                let depth = match kind {
                    OperationKind::Concat { depth } => depth,
                    _ => 1,
                };
                let mut slots = Vec::new();
                for source in operation.operands().skip(usize::from(extends)) {
                    if let Some(source) = self.heap_value(ctx, id, source) {
                        self.array_elements(source, depth, reader, &mut slots);
                    }
                }
                let sources = slots.len();
                let mut previous = None;
                while let Some(object) = self.next_object(target, previous) {
                    previous = Some(object);
                    let output = self.analysis.heap.objects[object].contents.slots()[0];
                    self.analysis.heap.cells[output].readers.insert(reader);
                    for &input in &slots[..sources] {
                        self.copy_slot(ctx, input, output, reader)?;
                    }
                    slots.push(output);
                    if extends {
                        if sources > 0 {
                            self.grow_array(object, None, reader);
                        }
                    } else {
                        self.widen_length(object, Range::of(Some(Integer::U32)));
                    }
                }
                self.reconcile_slot_group(ctx, &slots);
            }
            OperationKind::Mutate(Mutation::Truncate) => {
                let Some(base) = self.heap_value(ctx, id, operation.get_operand(0)) else {
                    return Ok(None);
                };
                self.analysis.heap.cells[base].readers.insert(reader);
                let mut previous = None;
                while let Some(object) = self.next_object(base, previous) {
                    previous = Some(object);
                    self.widen_length(object, Range::of(Some(Integer::U32)));
                }
            }
            OperationKind::Length => {
                let Some(base) = self.heap_value(ctx, id, operation.get_operand(0)) else {
                    return Ok(None);
                };
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
                return Ok(Some(ValueFacts::numeric(
                    Fact::Original,
                    self.analysis.heap.length(base),
                )));
            }
            OperationKind::Combine(kind) => {
                let Some(input) = self.heap_value(ctx, id, operation.get_operand(0)) else {
                    return Ok(None);
                };
                self.analysis.heap.cells[input].readers.insert(reader);
                let result = operation.get_result(0);
                let output = if kind == Combinator::Race {
                    None
                } else {
                    self.heap_allocation(ctx, id, result)?
                };
                let mut facts = ValueFacts::default();
                if kind != Combinator::Race {
                    facts.outcome = Outcomes::Fulfilled;
                }
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
                        facts.outcome = facts
                            .outcome
                            .join(self.analysis.heap.cells[element].facts.outcome);
                        if let Some(object) = output {
                            let slot = self.analysis.heap.objects[object].contents.slots()[0];
                            let mut facts = self.analysis.heap.cells[element].facts;
                            facts.outcome =
                                Outcomes::unknown(ctx, self.analysis.heap.cells[slot].ty);
                            self.update_slot(ctx, slot, facts);
                            self.copy_aliases(ctx, element, slot, reader)?;
                        } else {
                            facts.join(
                                self.analysis.heap.cells[element].facts,
                                Representation::Storage(None),
                            );
                            if let Some(result) = self.heap_value(ctx, id, result) {
                                self.copy_aliases(ctx, element, result, reader)?;
                            }
                        }
                    }
                    if let Some(object) = output {
                        self.widen_length(object, length);
                    }
                }
                if kind != Combinator::Race {
                    facts.domain = Fact::Original;
                    facts.range = Range::Unknown;
                }
                return Ok(Some(facts));
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
                        self.copy_aliases(ctx, from, to, reader)?;
                    }
                }
            }
            OperationKind::Intrinsic(Intrinsic::JsonSpread | Intrinsic::JsonOrderCopy) => {
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
            OperationKind::None
            | OperationKind::Stringify
            | OperationKind::Intrinsic(Intrinsic::JsonExclude | Intrinsic::JsonInsert) => {}
            _ => {
                // Unmodelled storage may retain aliases or mutate reachable objects.
                for value in operation.operands() {
                    if let Some(cell) = self.heap_value(ctx, id, value) {
                        self.open_aliases(cell, reader);
                    }
                }
                for value in operation.results() {
                    if let Some(cell) = self.heap_value(ctx, id, value) {
                        let object = self.heap_summary(ctx, value.get_type(ctx))?;
                        self.add_object(ctx, cell, object)?;
                    }
                }
            }
        }
        Ok(None)
    }
}
