//! Precise compiled roots and closed-aggregate pointer maps.
use crate::{
    function::FunctionLowering,
    repr::{Repr, repr},
    runtime::Runtime,
};
use anyhow::Result;
use perry_mir::{
    calls::{AsyncCallOp, CallOp},
    types::RecordType,
};
use perry_runtime::roots as root_stack;
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::MemKind;
use std::collections::{HashMap, HashSet};

/// Values live across `op`: its operands and what is live after it, except its results.
/// May repeat an operand that is also live after `op`.
fn live_across<'a>(
    ctx: &'a Context,
    op: Ptr<Operation>,
    after: &'a HashSet<Value>,
) -> impl Iterator<Item = Value> + 'a {
    let results: Vec<Value> = op.deref(ctx).results().collect();
    let operands: Vec<Value> = op.deref(ctx).operands().collect();
    after
        .iter()
        .copied()
        .filter(move |value| !results.contains(value))
        .chain(operands)
}

pub(super) fn export_checks(ctx: &mut Context, state: &mut crate::module::ModuleState) {
    use pliron::{
        builtin::{op_interfaces::SingleBlockRegionInterface, ops::FuncOp, types::FunctionType},
        op::Op,
    };
    use pliron_wasm::{control::ReturnOp, ops::set_export};
    for runtime in [Runtime::GcDebug, Runtime::GcCounter] {
        let (params, results) = runtime.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let export = crate::host::symbol("__", runtime.name());
        let function = FuncOp::new(ctx, export.clone(), ty);
        set_export(ctx, &function, export.as_ref());
        let mut args: Vec<_> = function
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        let mut b = FunctionLowering::glue(state, ctx, &function);
        let value = if runtime == Runtime::GcCounter {
            // Counter 7 counts compiled roots published so far, kept in a data cell.
            let seven = b.i32_const(ctx, 7);
            let compiled = b.num(
                ctx,
                pliron_wasm::attrs::NumOpcode::I32Eq,
                vec![args[0], seven],
            );
            let zero = b.i32_const(ctx, 0);
            args[0] = b.select(ctx, compiled, zero, args[0]);
            let counted = b.runtime(ctx, runtime, args);
            let address = b.i32_const(ctx, b.state.root_metrics.expect("checked counter") as i32);
            let count = b.load(ctx, MemKind::I64, address, 0);
            vec![b.select(ctx, compiled, count, counted)]
        } else {
            b.runtime_void(ctx, runtime, args);
            vec![]
        };
        let ret = ReturnOp::new(ctx, value).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, function.get_operation(), 0);
    }
}

/// A module uses the compact task allocator only when all remaining allocations are bounded.
/// Aggregate and builtin sites default to collection until a stronger lifetime proof exists.
pub(crate) fn needs_collector(
    ctx: &Context,
    functions: &[pliron::builtin::ops::FuncOp],
    summaries: &std::collections::HashMap<
        pliron::identifier::Identifier,
        perry_mir::effects::Summary,
    >,
) -> bool {
    use pliron::{builtin::op_interfaces::OneRegionInterface, linked_list::ContainsLinkedList};
    !crate::boundary::used_keys(ctx, functions).is_empty()
        || summaries
            .values()
            .any(|summary| summary.repeated || summary.unknown_helper_bound)
        || functions.iter().any(|function| {
            perry_mir::calls::is_async(ctx, function)
                || {
                    use pliron::builtin::op_interfaces::SymbolOpInterface;
                    function.get_symbol_name(ctx).as_ref() == perry_mir::globals::INITIALIZER
                }
                || function.get_region(ctx).deref(ctx).iter(ctx).any(|block| {
                    block.deref(ctx).iter(ctx).any(|op| {
                        Operation::get_op::<perry_mir::heap::AllocOp>(op, ctx).is_some()
                            || Operation::get_op::<perry_mir::variants::VariantNewOp>(op, ctx)
                                .is_some()
                            || Operation::get_op::<perry_mir::collections::ArrayNewOp>(op, ctx)
                                .is_some()
                    })
                })
        })
}

/// Explain the conservative allocation decisions for the optimized MIR actually emitted.
pub fn explain_tiers(
    ctx: &Context,
    module: &pliron::builtin::ops::ModuleOp,
    exports: &crate::Exports,
) -> String {
    use pliron::{
        builtin::op_interfaces::{OneRegionInterface, SymbolOpInterface},
        linked_list::ContainsLinkedList,
    };
    use std::fmt::Write;
    let functions = crate::analysis::reachable_functions(ctx, module, exports);
    let summaries = perry_mir::effects::summarize(ctx, &functions);
    let collectible = needs_collector(ctx, &functions, &summaries);
    let tier = if collectible {
        "instance (precise collection)"
    } else {
        "task (bounded calls)"
    };
    let mut report = format!("Allocation tiers after MIR optimization: {tier}\n");
    for function in &functions {
        let name = function.get_symbol_name(ctx);
        let summary = &summaries[&name];
        let sites = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .filter(|&op| perry_mir::effects::allocates(ctx, op))
            .count();
        let frames = function
            .get_region(ctx)
            .deref(ctx)
            .iter(ctx)
            .flat_map(|block| block.deref(ctx).iter(ctx).collect::<Vec<_>>())
            .filter_map(|op| Operation::get_op::<perry_mir::heap::AllocOp>(op, ctx))
            .filter(|allocation| allocation.tier(ctx) == perry_mir::attrs::Tier::Frame)
            .count();
        let leases = crate::lease::leased(ctx, function).len();
        writeln!(report, "  {name}: {sites} allocation sites; {frames} frame sites; repeated={}; unknown builtin bound={}; escaping parameters={:?}; {leases} reusable stream operations",
            summary.repeated, summary.unknown_helper_bound, summary.escapes).expect("string write");
    }
    report.push_str("  scalar: eliminated closed aggregates have no allocation site\n  frame: proven nonescaping scalar records reuse bounded activation storage; other aggregates use instance storage\n  buffer: host operations pin owners until acknowledgement\n");
    report
}

impl FunctionLowering<'_, '_> {
    pub(super) fn prepare_frame_storage(
        &mut self,
        ctx: &mut Context,
        function: &pliron::builtin::ops::FuncOp,
    ) -> Result<()> {
        use pliron::{
            builtin::op_interfaces::{OneRegionInterface, OneResultInterface},
            linked_list::ContainsLinkedList,
        };
        let mut bytes = 0;
        for block in function.get_region(ctx).deref(ctx).iter(ctx) {
            for op in block.deref(ctx).iter(ctx) {
                if let Some(allocation) = Operation::get_op::<perry_mir::heap::AllocOp>(op, ctx)
                    && allocation.tier(ctx) == perry_mir::attrs::Tier::Frame
                {
                    let record = TypedHandle::<RecordType>::from_handle(
                        allocation.get_result(ctx).get_type(ctx),
                        ctx,
                    )?;
                    self.frame_slots.insert(op, bytes);
                    bytes += perry_abi::layout::record::field(record.deref(ctx).fields()?.len());
                }
            }
        }
        if bytes != 0 {
            let size = self.i32_const(ctx, bytes as i32);
            let shape = self.i32_const(ctx, 0);
            let frame = self.runtime(ctx, Runtime::GcAlloc, vec![size, shape]);
            self.publish_roots(ctx, &[frame]);
            self.local_frame = Some(frame);
        }
        Ok(())
    }

    /// Run synchronous module initialization once. A failed or reentrant initializer traps
    /// consistently instead of exposing partially initialized typed values.
    pub(super) fn initialize_module(&mut self, ctx: &mut Context) {
        use pliron::op::Op;
        use pliron_wasm::{attrs::NumOpcode, control::UnreachableOp, ops::CallOp};
        let Some((function, address)) = self.state.initializer.clone() else {
            return;
        };
        let cell = self.i32_const(ctx, address as i32);
        let state = self.load(ctx, MemKind::I32, cell, 0);
        let two = self.i32_const(ctx, 2);
        let ready = self.num(ctx, NumOpcode::I32Eq, vec![state, two]);
        let done = self.new_block(ctx, vec![]);
        let check = self.new_block(ctx, vec![]);
        let start = self.new_block(ctx, vec![]);
        let failed = self.new_block(ctx, vec![]);
        self.cond_br(ctx, ready, (done, vec![]), (check, vec![]));
        self.block = check;
        self.cond_br(ctx, state, (failed, vec![]), (start, vec![]));
        self.block = failed;
        self.task_free(ctx);
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
        self.block = start;
        let one = self.i32_const(ctx, 1);
        self.store(ctx, MemKind::I32, cell, 0, one);
        let throws = self.state.throws.contains(&function);
        let results = if throws {
            vec![crate::repr::i32_type(ctx)]
        } else {
            vec![]
        };
        let call = CallOp::new(ctx, function, vec![], results).get_operation();
        self.insert(ctx, call);
        if throws {
            let initialized = self.new_block(ctx, vec![]);
            let status = call.deref(ctx).get_result(0);
            self.cond_br(ctx, status, (failed, vec![]), (initialized, vec![]));
            self.block = initialized;
        }
        self.store(ctx, MemKind::I32, cell, 0, two);
        self.br(ctx, done, vec![]);
        self.block = done;
    }

    fn root_stack(&mut self, ctx: &mut Context) -> Value {
        if let Some(stack) = self.root_stack {
            return stack;
        }
        // Late-discovered boundary roots must also dominate every exit and branch.
        let stack = self.entry_runtime(ctx, Runtime::RootsState);
        self.root_stack = Some(stack);
        stack
    }

    pub(super) fn root_mark(&mut self, ctx: &mut Context) -> Value {
        let stack = self.root_stack(ctx);
        self.load(ctx, MemKind::I32, stack, root_stack::LENGTH)
    }

    pub(super) fn reset_roots(&mut self, ctx: &mut Context, mark: Value) {
        let stack = self.root_stack(ctx);
        self.store(ctx, MemKind::I32, stack, root_stack::LENGTH, mark);
    }

    pub(super) fn publish_roots(&mut self, ctx: &mut Context, values: &[Value]) -> Value {
        use pliron_wasm::attrs::NumOpcode;
        let stack = self.root_stack(ctx);
        let mark = self.load(ctx, MemKind::I32, stack, root_stack::LENGTH);
        if values.is_empty() {
            return mark;
        }
        let count = u32::try_from(values.len()).expect("root count");
        assert!(count <= i32::MAX as u32 / 4, "root stack overflow");
        let count = self.i32_const(ctx, count as i32);
        let end = self.num(ctx, NumOpcode::I32Add, vec![mark, count]);
        let capacity = self.load(ctx, MemKind::I32, stack, root_stack::CAPACITY);
        let enough = self.num(ctx, NumOpcode::I32LeU, vec![end, capacity]);
        let ready = self.new_block(ctx, vec![]);
        let grow = self.new_block(ctx, vec![]);
        self.cond_br(ctx, enough, (ready, vec![]), (grow, vec![]));
        self.block = grow;
        self.runtime_void(ctx, Runtime::RootsReserve, vec![count]);
        self.br(ctx, ready, vec![]);
        self.block = ready;
        // Nested calls can relocate backing storage; only the descriptor and indices persist.
        let data = self.load(ctx, MemKind::I32, stack, root_stack::DATA);
        let two = self.i32_const(ctx, 2);
        let offset = self.num(ctx, NumOpcode::I32Shl, vec![mark, two]);
        let base = self.num(ctx, NumOpcode::I32Add, vec![data, offset]);
        for (index, &value) in values.iter().enumerate() {
            self.store(ctx, MemKind::I32, base, index as u32 * 4, value);
        }
        self.store(ctx, MemKind::I32, stack, root_stack::LENGTH, end);
        if let Some(address) = self.state.root_metrics {
            let counter = self.i32_const(ctx, address as i32);
            let before = self.load(ctx, MemKind::I64, counter, 0);
            let count = self.i64_const(ctx, values.len() as u64);
            let total = self.num(ctx, NumOpcode::I64Add, vec![before, count]);
            self.store(ctx, MemKind::I64, counter, 0, total);
        }
        mark
    }

    /// Enter the current export's task, and with it the collector: the task's scheduler
    /// roots register, and resources released since the last entry are dropped.
    pub(super) fn task_enter(&mut self, ctx: &mut Context) -> Value {
        let task = self.runtime(ctx, Runtime::TaskEnter, vec![]);
        if self.state.collectible {
            if self.state.memory_checks {
                let flags = self.i32_const(ctx, 3);
                self.runtime_void(ctx, Runtime::GcDebug, vec![flags]);
            }
            self.runtime_void(ctx, Runtime::GcTaskEnter, vec![]);
            self.drain_resources(ctx);
        }
        task
    }

    /// Release the current task's storage; the collector unregisters its roots first and may
    /// then reclaim an empty instance heap.
    pub(super) fn task_free(&mut self, ctx: &mut Context) {
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::GcTaskLeave, vec![]);
        }
        self.runtime_void(ctx, Runtime::TaskFree, vec![]);
        if self.state.collectible {
            self.runtime_void(ctx, Runtime::GcTaskEnd, vec![]);
        }
    }

    /// Task storage of `size` bytes. While canonical arguments are borrowed, the area is a
    /// root until the boundary code releases it.
    pub(super) fn alloc(&mut self, ctx: &mut Context, size: Value) -> Value {
        let area = self.runtime(ctx, Runtime::Alloc, vec![size]);
        if self.canonical_borrows {
            self.publish_roots(ctx, &[area]);
        }
        area
    }

    pub(super) fn enter_roots(&mut self, ctx: &mut Context) {
        if self.state.collectible {
            self.root_base = Some(self.root_mark(ctx));
        }
    }

    pub(super) fn leave_roots(&mut self, ctx: &mut Context) {
        if let Some(base) = self.root_base {
            self.reset_roots(ctx, base);
        }
        if let Some(frame) = self.local_frame {
            self.runtime_void(ctx, Runtime::FrameFree, vec![frame]);
        }
    }

    pub(super) fn safe_point(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        after: &HashSet<Value>,
        order: &HashMap<Value, usize>,
    ) -> Result<Option<Value>> {
        let collecting_call = Operation::get_op::<CallOp>(op, ctx).is_some_and(|call| {
            self.state
                .call_effects
                .get(&call.callee(ctx))
                .is_none_or(|effect| effect.may_collect)
        });
        if !self.state.collectible
            || !(perry_mir::effects::allocates(ctx, op)
                || collecting_call
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some())
        {
            return Ok(None);
        }
        let mut live: Vec<Value> = live_across(ctx, op, after)
            .filter(|value| repr(ctx, value.get_type(ctx)).is_ok_and(|kind| kind == Repr::Ref))
            .collect();
        live.sort_unstable_by_key(|value| order[value]);
        live.dedup();
        let values = live
            .into_iter()
            .map(|value| self.mapped(ctx, value))
            .collect::<Result<Vec<_>>>()?;
        let atomic = self.state.resources.is_empty()
            && perry_mir::effects::allocation(ctx, op) == perry_mir::effects::Allocation::Atomic;
        let resume = atomic.then(|| {
            let due = self.runtime(ctx, Runtime::GcDue, vec![]);
            let collect = self.new_block(ctx, vec![]);
            let resume = self.new_block(ctx, vec![]);
            self.cond_br(ctx, due, (collect, vec![]), (resume, vec![]));
            self.block = collect;
            resume
        });
        let mark = (!values.is_empty()).then(|| self.publish_roots(ctx, &values));
        self.runtime_void(ctx, Runtime::GcCheck, vec![]);
        self.drain_resources(ctx);
        if let Some(resume) = resume {
            self.end_safe_point(ctx, mark);
            self.br(ctx, resume, vec![]);
            self.block = resume;
            return Ok(None);
        }
        Ok(mark)
    }

    pub(super) fn end_safe_point(&mut self, ctx: &mut Context, mark: Option<Value>) {
        if let Some(mark) = mark {
            self.reset_roots(ctx, mark);
        }
    }

    pub(super) fn clear_dead_frame_slots(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        after: &HashSet<Value>,
    ) {
        if !self.state.collectible {
            return;
        }
        let Some(frame) = &self.frame else {
            return;
        };
        let operands: Vec<Value> = op.deref(ctx).operands().collect();
        let results: Vec<Value> = op.deref(ctx).results().collect();
        let live = |value: &Value| {
            operands.contains(value) || (after.contains(value) && !results.contains(value))
        };
        let pointer = frame.frame;
        let mut dead: Vec<_> = frame
            .slots
            .iter()
            .filter_map(|(value, (offset, _))| {
                (!live(value) && repr(ctx, value.get_type(ctx)).is_ok_and(|kind| kind == Repr::Ref))
                    .then_some(*offset)
            })
            .collect();
        dead.sort_unstable();
        if !dead.is_empty() {
            let zero = self.i32_const(ctx, 0);
            for offset in dead {
                self.store(ctx, MemKind::I32, pointer, offset, zero);
            }
        }
    }

    pub(super) fn pointer_shape(&mut self, offsets: &[u32]) -> u32 {
        if offsets.is_empty() {
            return 0;
        }
        let address = self.state.data.align(4);
        for word in [0, offsets.len() as u32]
            .into_iter()
            .chain(offsets.iter().copied())
        {
            self.state.data.bytes.extend_from_slice(&word.to_le_bytes());
        }
        address
    }

    pub(super) fn new_promise(&mut self, ctx: &mut Context, payload: TypeHandle) -> Result<Value> {
        if self.state.collectible {
            let references = self.i32_const(ctx, i32::from(repr(ctx, payload)? == Repr::Ref));
            Ok(self.runtime(ctx, Runtime::PromiseNewCollectible, vec![references]))
        } else {
            Ok(self.runtime(ctx, Runtime::PromiseNew, vec![]))
        }
    }

    pub(super) fn record_storage(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        size: Value,
    ) -> Result<Value> {
        if !self.state.collectible {
            return Ok(self.alloc(ctx, size));
        }
        let shape = if let Some(&shape) = self.state.pointer_maps.get(&ty) {
            shape
        } else {
            let record = TypedHandle::<RecordType>::from_handle(ty, ctx)?;
            let mut offsets: Vec<_> = record
                .deref(ctx)
                .fields()?
                .iter()
                .enumerate()
                .filter_map(|(index, (_, ty))| {
                    repr(ctx, *ty)
                        .is_ok_and(|kind| kind == Repr::Ref)
                        .then_some(perry_abi::layout::record::field(index))
                })
                .collect();
            if self.state.json_provenance {
                offsets.push(perry_abi::layout::record::SOURCE);
            }
            let shape = if offsets.is_empty() {
                0
            } else {
                self.pointer_shape(&offsets)
            };
            self.state.pointer_maps.insert(ty, shape);
            shape
        };
        let shape = self.i32_const(ctx, shape as i32);
        Ok(self.runtime(ctx, Runtime::GcAlloc, vec![size, shape]))
    }

    pub(super) fn new_array(
        &mut self,
        ctx: &mut Context,
        capacity: Value,
        element: TypeHandle,
    ) -> Result<Value> {
        if self.state.collectible {
            let references = self.i32_const(ctx, i32::from(repr(ctx, element)? == Repr::Ref));
            Ok(self.runtime(
                ctx,
                Runtime::ArrayNewCollectible,
                vec![capacity, references],
            ))
        } else {
            Ok(self.runtime(ctx, Runtime::ArrayNew, vec![capacity]))
        }
    }
}
