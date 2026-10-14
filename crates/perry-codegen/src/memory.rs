//! Precise compiled roots and closed-aggregate pointer maps.
use crate::{
    function::FunctionLowering,
    repr::{Repr, i32_type, repr},
    runtime::Runtime,
};
use anyhow::{Result, anyhow};
use perry_abi::layout::{record, roots as root_stack};
use perry_mir::{
    calls::{AsyncCallOp, CallOp},
    types::RecordType,
};
use pliron::{
    context::{Context, Ptr},
    op::Op,
    operation::Operation,
    pass::AnalysisManager,
    printable::Printable,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::MemKind;
use std::collections::HashMap;

pub(super) fn export_checks(ctx: &mut Context, state: &mut crate::module::ModuleState) {
    use pliron::{
        builtin::{op_interfaces::SingleBlockRegionInterface, ops::FuncOp, types::FunctionType},
        op::Op,
    };
    use pliron_wasm::{control::ReturnOp, ops::set_export};
    for runtime in [Runtime::GcDebug, Runtime::GcCounter] {
        let (params, results) = runtime.signature(ctx);
        let ty = FunctionType::get(ctx, params, results);
        let export = crate::host::symbol(pliron::ident!("__"), runtime.name());
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
            let address = b.i32_const(ctx, crate::DATA_BASE as i32);
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

/// Task regions suffice when allocations do not repeat, suspend or outlive the task.
/// Resource-bearing boundaries and checked builds select collection at their callers.
pub(crate) fn needs_collector(
    ctx: &Context,
    functions: &[pliron::builtin::ops::FuncOp],
    summaries: &perry_mir::facts::ProgramFacts,
) -> bool {
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    functions.iter().any(|function| {
        let name = function.get_symbol_name(ctx);
        let effects = &summaries.effects(&name).expect("function effects").effects;
        name.as_ref() == perry_mir::globals::INITIALIZER
            || effects.repeated_allocations
            || effects.unknown_helper_bound
            || effects.may_suspend
            || effects.may_reenter
    })
}

/// Explain the conservative allocation decisions for the optimized MIR actually emitted.
pub fn explain_tiers(
    ctx: &Context,
    module: &pliron::builtin::ops::ModuleOp,
    exports: &crate::Exports,
    analyses: &mut AnalysisManager,
) -> Result<String> {
    use pliron::builtin::op_interfaces::SymbolOpInterface;
    use std::fmt::Write;
    let functions = crate::analysis::reachable_functions(ctx, module, exports);
    analyses
        .get_analysis::<perry_mir::liveness::LiveValues>(module.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    analyses
        .get_analysis::<perry_mir::facts::ProgramFacts>(module.get_operation(), ctx)
        .map_err(|error| anyhow!("{}", error.disp(ctx)))?;
    let effects = analyses
        .try_get_analysis::<perry_mir::facts::ProgramFacts>(module.get_operation())
        .expect("runtime effects");
    let summaries = &*effects;
    let live = analyses
        .try_get_analysis::<perry_mir::liveness::LiveValues>(module.get_operation())
        .expect("liveness");
    let collectible = !crate::boundary::used_keys(ctx, &functions).is_empty()
        || needs_collector(ctx, &functions, summaries);
    let tier = if collectible {
        "instance (precise collection)"
    } else {
        "task (bounded calls)"
    };
    let mut report = format!("Allocation tiers after MIR optimization: {tier}\n");
    let stats = perry_mir::pipeline::statistics(module.get_operation(), ctx);
    writeln!(report, "Optimization statistics: numeric_contexts={}, constraint_visits={}, specializations={}, cleanup_rounds={}, summarized_heap_sets={}, summarized_contexts={}, range_widenings={}, parse_stringify_rewrites={}, parse_stringify_missed={}", stats.numeric_contexts, stats.constraint_visits, stats.specializations, stats.cleanup_rounds, stats.summarized_heap_sets, stats.summarized_contexts, stats.range_widenings, stats.parse_stringify_rewrites, stats.parse_stringify_missed).expect("string write");
    for function in &functions {
        let name = function.get_symbol_name(ctx);
        let summary = summaries.effects(&name).expect("function effects");
        let ops = crate::analysis::function_ops(ctx, function);
        let sites = ops
            .iter()
            .filter(|&&op| perry_mir::effects::allocates(ctx, op))
            .count();
        let frames = ops
            .into_iter()
            .filter_map(|op| Operation::get_op::<perry_mir::heap::AllocOp>(op, ctx))
            .filter(|allocation| allocation.tier(ctx) == perry_mir::attrs::Tier::Frame)
            .count();
        let leases = crate::lease::leased(ctx, function, &live).len();
        writeln!(report, "  {name}: {sites} allocation sites; {frames} frame sites; repeated allocations={}; unknown builtin bound={}; escaping parameters={:?}; {leases} reusable stream operations",
            summary.effects.repeated_allocations, summary.effects.unknown_helper_bound, summary.escapes).expect("string write");
    }
    report.push_str("  scalar: eliminated closed aggregates have no allocation site\n  frame: proven nonescaping scalar records reuse bounded activation storage; other aggregates use the selected task or instance storage\n  buffer: host operations pin owners until acknowledgement\n");
    Ok(report)
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
                    bytes += record::field(record.deref(ctx).fields()?.len());
                }
            }
        }
        if bytes != 0 {
            let size = self.i32_const(ctx, bytes as i32);
            let frame = if self.state.collectible {
                let shape = self.i32_const(ctx, 0);
                let frame = self.runtime(ctx, Runtime::GcAlloc, vec![size, shape]);
                if !self.atomic_construction {
                    self.publish_roots(ctx, &[frame]);
                }
                frame
            } else {
                self.alloc(ctx, size)
            };
            self.local_frame = Some(frame);
        }
        Ok(())
    }

    /// Run synchronous module initialization once. A failed or reentrant initializer traps
    /// consistently instead of exposing partially initialized typed values.
    pub(super) fn initialize_module(&mut self, ctx: &mut Context) {
        use pliron_wasm::control::UnreachableOp;
        let Some(status) = self.initialization_status(ctx) else {
            return;
        };
        let failed = self.new_block(ctx, vec![]);
        let ready = self.new_block(ctx, vec![]);
        self.cond_br(ctx, status, (failed, vec![]), (ready, vec![]));
        self.block = failed;
        self.task_free(ctx);
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
        self.block = ready;
    }

    /// Initialize once, retaining a failed exception as an instance root for later callers.
    pub(super) fn initialization_status(&mut self, ctx: &mut Context) -> Option<Value> {
        use pliron_wasm::{attrs::NumOpcode, ops::CallOp};
        let Some(crate::module::Initializer::Sync(address)) = self.state.initializer else {
            return None;
        };
        let function = crate::module::INITIALIZER;
        let cell = self.i32_const(ctx, address as i32);
        let error_cell = self.i32_const(ctx, (address + 16) as i32);
        self.runtime_void(ctx, Runtime::GcGlobal, vec![error_cell]);
        let state = self.load(ctx, MemKind::I32, cell, 0);
        let two = self.i32_const(ctx, 2);
        let ready = self.num(ctx, NumOpcode::I32Eq, vec![state, two]);
        let i32 = i32_type(ctx);
        let done = self.new_block(ctx, vec![i32]);
        let check = self.new_block(ctx, vec![]);
        let start = self.new_block(ctx, vec![]);
        let failed = self.new_block(ctx, vec![]);
        let zero = self.i32_const(ctx, 0);
        self.cond_br(ctx, ready, (done, vec![zero]), (check, vec![]));
        self.block = check;
        self.cond_br(ctx, state, (failed, vec![]), (start, vec![]));
        self.block = failed;
        let exception = self.load(ctx, MemKind::I32, error_cell, 0);
        let existing = self.new_block(ctx, vec![]);
        let reentrant = self.new_block(ctx, vec![]);
        let raised = self.new_block(ctx, vec![i32]);
        self.cond_br(ctx, exception, (existing, vec![]), (reentrant, vec![]));
        self.block = reentrant;
        let kind = self.i32_const(ctx, 6);
        let tag = self.i32_const(ctx, 3);
        let message = self
            .state
            .data
            .string("Module initialization is still in progress");
        let message = self.i64_const(ctx, message as u64);
        let cycle = self.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message]);
        self.br(ctx, raised, vec![cycle]);
        self.block = existing;
        self.br(ctx, raised, vec![exception]);
        self.block = raised;
        let exception = raised.deref(ctx).get_argument(0);
        let status = self.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
        self.br(ctx, done, vec![status]);
        self.block = start;
        let one = self.i32_const(ctx, 1);
        self.store(ctx, MemKind::I32, cell, 0, one);
        let throws = self.state.may_throw(&function);
        let results = if throws { vec![i32] } else { vec![] };
        let call = CallOp::new(ctx, function, vec![], results).get_operation();
        self.insert(ctx, call);
        if throws {
            let status = call.deref(ctx).get_result(0);
            let initialized = self.new_block(ctx, vec![]);
            let rejected = self.new_block(ctx, vec![]);
            self.cond_br(ctx, status, (rejected, vec![]), (initialized, vec![]));
            self.block = rejected;
            let exception = self.runtime(ctx, Runtime::ExceptionCatch, vec![status]);
            self.store(ctx, MemKind::I32, error_cell, 0, exception);
            let three = self.i32_const(ctx, 3);
            self.store(ctx, MemKind::I32, cell, 0, three);
            self.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
            self.br(ctx, done, vec![status]);
            self.block = initialized;
        }
        self.store(ctx, MemKind::I32, cell, 0, two);
        self.br(ctx, done, vec![zero]);
        self.block = done;
        Some(done.deref(ctx).get_argument(0))
    }

    /// Start asynchronous initialization once and share its fulfillment or rejection.
    pub(super) fn initialization_promise(&mut self, ctx: &mut Context) -> Result<Value> {
        use pliron_wasm::ops::CallOp;
        let Some(crate::module::Initializer::Async(address)) = self.state.initializer else {
            anyhow::bail!("module_initialize without an async initializer");
        };
        let function = crate::module::INITIALIZER;
        let cell = self.i32_const(ctx, address as i32);
        self.runtime_void(ctx, Runtime::GcGlobal, vec![cell]);
        let previous = self.load(ctx, MemKind::I32, cell, 0);
        let i32 = i32_type(ctx);
        let start = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![i32]);
        self.cond_br(ctx, previous, (done, vec![previous]), (start, vec![]));
        self.block = start;
        let call = CallOp::new(ctx, function, vec![], vec![i32]).get_operation();
        self.insert(ctx, call);
        let promise = call.deref(ctx).get_result(0);
        self.store(ctx, MemKind::I32, cell, 0, promise);
        self.br(ctx, done, vec![promise]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
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
        if self.state.count_roots {
            let counter = self.i32_const(ctx, crate::DATA_BASE as i32);
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
        if self.state.collectible
            && let Some(frame) = self.local_frame
        {
            self.runtime_void(ctx, Runtime::FrameFree, vec![frame]);
        }
    }

    pub(super) fn safe_point(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        after: perry_mir::liveness::LiveAfter<'_>,
        order: &HashMap<Value, usize>,
    ) -> Result<Option<Value>> {
        if self.atomic_construction {
            return Ok(None);
        }
        let call_effect = Operation::get_op::<CallOp>(op, ctx)
            .and_then(|call| self.state.program_facts.effects(&call.callee(ctx)));
        let atomic_call = call_effect.is_some_and(|e| e.effects.atomic_construction);
        let collecting_call = Operation::get_op::<CallOp>(op, ctx).is_some()
            && call_effect
                .is_none_or(|effect| effect.effects.may_collect || effect.effects.allocates);
        if !self.state.collectible
            || !(perry_mir::effects::allocates(ctx, op)
                || collecting_call
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some())
        {
            return Ok(None);
        }
        self.clear_dead_frame_slots(ctx, op, after);
        let mut live: Vec<Value> = {
            let operation = op.deref(ctx);
            after
                .values()
                .filter(|value| !operation.results().any(|result| result == *value))
                .chain(operation.operands())
                .filter(|value| repr(ctx, value.get_type(ctx)).is_ok_and(|kind| kind == Repr::Ref))
                .collect()
        };
        live.sort_unstable_by_key(|value| order[value]);
        live.dedup();
        let values = live
            .into_iter()
            .map(|value| self.mapped(ctx, value))
            .collect::<Result<Vec<_>>>()?;
        let atomic = self.state.resources.is_empty()
            && (perry_mir::effects::allocation(ctx, op) == perry_mir::effects::Allocation::Atomic
                || atomic_call);
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
        after: perry_mir::liveness::LiveAfter<'_>,
    ) {
        if !self.state.collectible {
            return;
        }
        let Some(frame) = &self.frame else {
            return;
        };
        let live = |value: &Value| {
            op.deref(ctx).operands().any(|operand| operand == *value)
                || (after.contains(value)
                    && !op.deref(ctx).results().any(|result| result == *value))
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
                        .then_some(record::field(index))
                })
                .collect();
            if self.state.retains_json {
                offsets.push(record::JSON);
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
