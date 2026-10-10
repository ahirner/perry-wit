//! Precise compiled roots and closed-aggregate pointer maps.
use crate::{FunctionLowering, Repr, Runtime, W, repr};
use anyhow::Result;
use perry_mir::{
    calls::{AsyncCallOp, CallOp},
    types::RecordType,
};
use pliron::{
    context::{Context, Ptr},
    operation::Operation,
    r#type::{TypeHandle, Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::attrs::MemKind;
use std::collections::HashSet;

pub(super) fn export_checks(ctx: &mut Context, state: &mut crate::ModuleState) {
    use pliron::{
        builtin::{op_interfaces::SingleBlockRegionInterface, ops::FuncOp, types::FunctionType},
        op::Op,
    };
    use pliron_wasm::{control::ReturnOp, ops::set_export};
    for (name, result) in [("perry_gc_debug", None), ("perry_gc_counter", Some(W::I64))] {
        let params = vec![crate::i32_type(ctx)];
        let results = result
            .map(|_| {
                pliron::builtin::types::IntegerType::get(
                    ctx,
                    64,
                    pliron::builtin::types::Signedness::Signless,
                )
                .into()
            })
            .into_iter()
            .collect();
        let ty = FunctionType::get(ctx, params, results);
        let export = format!("__{name}");
        let function = FuncOp::new(ctx, export.clone().try_into().expect("counter name"), ty);
        set_export(ctx, &function, &export);
        let args = function
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        let mut b = FunctionLowering::glue(state, ctx, &function);
        let result_types: &'static [W] = if result.is_some() { &[W::I64] } else { &[] };
        let value = b.call(ctx, name, &[W::I32], result_types, args);
        let ret = ReturnOp::new(ctx, value.into_iter().collect()).get_operation();
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
    !crate::boundary::resources::used_keys(ctx, functions).is_empty()
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
pub fn explain_tiers(ctx: &Context, module: &pliron::builtin::ops::ModuleOp) -> String {
    use pliron::{
        builtin::op_interfaces::{OneRegionInterface, SymbolOpInterface},
        linked_list::ContainsLinkedList,
    };
    use std::fmt::Write;
    let functions = crate::module_functions(ctx, module);
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

impl FunctionLowering<'_> {
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
                    bytes += 8 + 8 * record.deref(ctx).fields()?.len() as u32;
                }
            }
        }
        if bytes != 0 {
            let size = self.i32_const(ctx, bytes as i32);
            let shape = self.i32_const(ctx, 0);
            let frame = self
                .call(
                    ctx,
                    "perry_gc_alloc",
                    &[W::I32, W::I32],
                    &[W::I32],
                    vec![size, shape],
                )
                .expect("frame");
            self.call(ctx, "perry_roots_push", &[W::I32], &[], vec![frame]);
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
        self.runtime(ctx, Runtime::TaskFree, vec![]);
        let trap = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, trap);
        self.block = start;
        let one = self.i32_const(ctx, 1);
        self.store(ctx, MemKind::I32, cell, 0, one);
        let throws = self.state.throws.contains(&function);
        let results = if throws {
            vec![crate::i32_type(ctx)]
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

    pub(super) fn enter_roots(&mut self, ctx: &mut Context) {
        if self.state.collectible {
            self.root_base = self.call(ctx, "perry_roots_mark", &[], &[W::I32], vec![]);
        }
    }

    pub(super) fn leave_roots(&mut self, ctx: &mut Context) {
        if let Some(base) = self.root_base {
            self.call(ctx, "perry_roots_reset", &[W::I32], &[], vec![base]);
        }
        if let Some(frame) = self.local_frame {
            self.call(ctx, "perry_frame_free", &[W::I32], &[], vec![frame]);
        }
    }

    pub(super) fn safe_point(
        &mut self,
        ctx: &mut Context,
        op: Ptr<Operation>,
        after: &HashSet<Value>,
    ) -> Result<Option<Value>> {
        if !self.state.collectible
            || !(perry_mir::effects::allocates(ctx, op)
                || Operation::get_op::<CallOp>(op, ctx).is_some()
                || Operation::get_op::<AsyncCallOp>(op, ctx).is_some())
        {
            return Ok(None);
        }
        let mut live = after.clone();
        for result in op.deref(ctx).results() {
            live.remove(&result);
        }
        live.extend(op.deref(ctx).operands());
        let mut live: Vec<_> = live
            .into_iter()
            .filter(|value| repr(ctx, value.get_type(ctx)).is_ok_and(|kind| kind == Repr::Ref))
            .collect();
        live.sort_by_key(|value| format!("{value:?}"));
        let mark = self
            .call(ctx, "perry_roots_mark", &[], &[W::I32], vec![])
            .expect("mark");
        for value in live {
            let value = self.mapped(ctx, value)?;
            self.call(ctx, "perry_roots_push", &[W::I32], &[], vec![value]);
        }
        self.call(ctx, "perry_gc_check", &[], &[], vec![]);
        self.drain_resources(ctx);
        Ok(Some(mark))
    }

    pub(super) fn end_safe_point(&mut self, ctx: &mut Context, mark: Option<Value>) {
        if let Some(mark) = mark {
            self.call(ctx, "perry_roots_reset", &[W::I32], &[], vec![mark]);
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
        let mut live = after.clone();
        for result in op.deref(ctx).results() {
            live.remove(&result);
        }
        live.extend(op.deref(ctx).operands());
        let pointer = frame.frame;
        let mut dead: Vec<_> = frame
            .slots
            .iter()
            .filter_map(|(value, (offset, _))| {
                (!live.contains(value)
                    && repr(ctx, value.get_type(ctx)).is_ok_and(|kind| kind == Repr::Ref))
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
        while !self.state.data.bytes.len().is_multiple_of(4) {
            self.state.data.bytes.push(0);
        }
        let address = crate::DATA_BASE + self.state.data.bytes.len() as u32;
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
            Ok(self
                .call(
                    ctx,
                    "perry_promise_new_collectible",
                    &[W::I32],
                    &[W::I32],
                    vec![references],
                )
                .expect("promise"))
        } else {
            Ok(self
                .runtime(ctx, Runtime::PromiseNew, vec![])
                .expect("promise"))
        }
    }

    pub(super) fn record_storage(
        &mut self,
        ctx: &mut Context,
        ty: TypeHandle,
        size: Value,
    ) -> Result<Value> {
        if !self.state.collectible {
            return Ok(self
                .runtime(ctx, Runtime::Alloc, vec![size])
                .expect("record"));
        }
        let shape = if let Some(&shape) = self.state.pointer_maps.get(&ty) {
            shape
        } else {
            let record = TypedHandle::<RecordType>::from_handle(ty, ctx)?;
            let fields = record.deref(ctx).fields()?.to_vec();
            let offsets: Vec<_> = fields
                .iter()
                .enumerate()
                .filter_map(|(index, (_, ty))| {
                    repr(ctx, *ty)
                        .is_ok_and(|kind| kind == Repr::Ref)
                        .then_some(8 + 8 * index as u32)
                })
                .collect();
            let shape = self.pointer_shape(&offsets);
            self.state.pointer_maps.insert(ty, shape);
            shape
        };
        let shape = self.i32_const(ctx, shape as i32);
        Ok(self
            .call(
                ctx,
                "perry_gc_alloc",
                &[W::I32, W::I32],
                &[W::I32],
                vec![size, shape],
            )
            .expect("record"))
    }

    pub(super) fn new_array(
        &mut self,
        ctx: &mut Context,
        capacity: Value,
        element: TypeHandle,
    ) -> Result<Value> {
        if self.state.collectible {
            let references = self.i32_const(ctx, i32::from(repr(ctx, element)? == Repr::Ref));
            Ok(self
                .call(
                    ctx,
                    "perry_array_new_collectible",
                    &[W::I32, W::I32],
                    &[W::I32],
                    vec![capacity, references],
                )
                .expect("array"))
        } else {
            Ok(self
                .runtime(ctx, Runtime::ArrayNew, vec![capacity])
                .expect("array"))
        }
    }
}
