//! The async split (`TODOs.md` P3.8): stackless state machines for `async` functions.
//!
//! Each async MIR function `f` becomes two Wasm functions:
//!
//! - the **ramp** `f(args) -> promise`: allocates a frame in the task region, stores the
//!   arguments, creates the result promise and runs `step_f` eagerly;
//! - the **step** `step_f(frame)`: the function body. Its entry dispatches on the frame's
//!   state: 0 starts the body, `k` resumes after the `k`-th `await`.
//!
//! An `await` of a settled promise continues inline. Otherwise the step stores its state,
//! registers the frame as a waiter on the promise and returns; settling the promise queues the
//! frame, and the drive loop (`drive`, run at every entry from the host) dispatches queued
//! frames by kind. Values live across an `await` (backward liveness over the MIR CFG) live in
//! frame slots: they are stored where they are defined and loaded where they are used, so the
//! resume entries need no SSA repair. `return v` fulfils the frame's promise, and an exception
//! rejects it with its status.
use crate::{FunctionLowering, ModuleState, Runtime, i32_type, slot_kind, wasm_type};
use anyhow::{Result, anyhow};
use perry_mir::calls::AwaitOp;
use perry_runtime::promise as promises;
use pliron::{
    basic_block::BasicBlock,
    builtin::{
        op_interfaces::{OneRegionInterface, SingleBlockRegionInterface, SymbolOpInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::{FP64Type, FunctionType},
    },
    context::{Context, Ptr},
    identifier::Identifier,
    linked_list::ContainsLinkedList,
    op::Op,
    operation::Operation,
    r#type::{Typed, TypedHandle},
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::{ReturnOp, UnreachableOp},
    ops::CallOp,
};
use std::collections::{HashMap, HashSet};

/// Frame header: dispatch kind, resume state, result promise, awaited promise.
pub(crate) const KIND: u32 = 0;
pub(crate) const STATE: u32 = 4;
pub(crate) const PROMISE: u32 = 8;
pub(crate) const AWAITED: u32 = 12;
const SLOTS: u32 = 16;

/// The frame of the async function being lowered.
pub(crate) struct Frame {
    pub(crate) frame: Value,
    /// Frame slot offset and access of each MIR value kept in the frame.
    pub(crate) slots: HashMap<Value, (u32, MemKind)>,
    /// Continuation block of each `await`, in state order (state = index + 1).
    pub(crate) resumes: Vec<Ptr<BasicBlock>>,
}

pub(crate) fn step_name(name: &Identifier) -> Identifier {
    format!("step_{name}").try_into().expect("identifier")
}

pub(crate) fn drive_name() -> Identifier {
    "drive".try_into().expect("identifier")
}

/// MIR values live across some `await` of `function`, in a deterministic order.
fn live_across_awaits(ctx: &Context, function: &FuncOp) -> Vec<Value> {
    let blocks: Vec<_> = function.get_region(ctx).deref(ctx).iter(ctx).collect();
    let mut live_in: HashMap<Ptr<BasicBlock>, HashSet<Value>> = HashMap::new();
    let transfer = |ctx: &Context, ops: &[Ptr<Operation>], mut live: HashSet<Value>| {
        for op in ops.iter().rev() {
            for result in op.deref(ctx).results() {
                live.remove(&result);
            }
            live.extend(op.deref(ctx).operands());
        }
        live
    };
    loop {
        let mut changed = false;
        for &block in blocks.iter().rev() {
            let mut live: HashSet<Value> = HashSet::new();
            for succ in block.deref(ctx).succs(ctx) {
                live.extend(live_in.get(&succ).into_iter().flatten().copied());
            }
            let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
            let mut live = transfer(ctx, &ops, live);
            for arg in block.deref(ctx).arguments() {
                live.remove(&arg);
            }
            if live_in.get(&block) != Some(&live) {
                live_in.insert(block, live);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let mut across: Vec<Value> = Vec::new();
    let mut seen = HashSet::new();
    for &block in &blocks {
        let ops: Vec<_> = block.deref(ctx).iter(ctx).collect();
        let mut live_out: HashSet<Value> = HashSet::new();
        for succ in block.deref(ctx).succs(ctx) {
            live_out.extend(live_in.get(&succ).into_iter().flatten().copied());
        }
        for (index, &op) in ops.iter().enumerate() {
            if Operation::get_op::<AwaitOp>(op, ctx).is_none() {
                continue;
            }
            let mut live = transfer(ctx, &ops[index + 1..], live_out.clone());
            live.remove(&op.deref(ctx).get_result(0));
            let mut live: Vec<_> = live.into_iter().collect();
            // Order by definition position for deterministic frame layouts.
            live.sort_by_key(|v| format!("{v:?}"));
            for value in live {
                if seen.insert(value) {
                    across.push(value);
                }
            }
        }
    }
    across
}

/// Lower async MIR function `mir` with dispatch kind `kind`: emit its ramp and step functions.
pub(crate) fn lower(
    ctx: &mut Context,
    state: &mut ModuleState,
    mir: &FuncOp,
    kind: i32,
) -> Result<()> {
    let name = mir.get_symbol_name(ctx);
    let ty = TypedHandle::<FunctionType>::from_handle(mir.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let entry = mir.get_entry_block(ctx);
    let params: Vec<Value> = entry.deref(ctx).arguments().collect();

    // Frame layout: parameters first, then every other value live across an await.
    let mut slots: HashMap<Value, (u32, MemKind)> = HashMap::new();
    let mut offset = SLOTS;
    for (param, ty) in params.iter().zip(&mir_params) {
        slots.insert(*param, (offset, slot_kind(ctx, *ty)?));
        offset += 8;
    }
    for value in live_across_awaits(ctx, mir) {
        if let std::collections::hash_map::Entry::Vacant(slot) = slots.entry(value) {
            slot.insert((offset, slot_kind(ctx, value.get_type(ctx))?));
            offset += 8;
        }
    }
    let frame_size = offset;

    // Ramp: allocate the frame, store the arguments, run the body to its first suspension.
    let wasm_params = mir_params
        .iter()
        .map(|t| wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    let i32 = i32_type(ctx);
    let ramp_ty = FunctionType::get(ctx, wasm_params, vec![i32]);
    let ramp = FuncOp::new(ctx, name.clone(), ramp_ty);
    {
        let args: Vec<_> = ramp.get_entry_block(ctx).deref(ctx).arguments().collect();
        let mut b = FunctionLowering::glue(state, ctx, &ramp);
        let size = b.i32_const(ctx, frame_size as i32);
        let frame = b.runtime(ctx, Runtime::Alloc, vec![size]).expect("frame");
        let kind_value = b.i32_const(ctx, kind);
        b.store(ctx, MemKind::I32, frame, KIND, kind_value);
        let zero = b.i32_const(ctx, 0);
        b.store(ctx, MemKind::I32, frame, STATE, zero);
        let promise = b
            .runtime(ctx, Runtime::PromiseNew, vec![])
            .expect("promise");
        b.store(ctx, MemKind::I32, frame, PROMISE, promise);
        for (param, arg) in params.iter().zip(args) {
            let (at, access) = slots[param];
            b.store(ctx, access, frame, at, arg);
        }
        let call = CallOp::new(ctx, step_name(&name), vec![frame], vec![]).get_operation();
        b.insert(ctx, call);
        let ret = ReturnOp::new(ctx, vec![promise]).get_operation();
        b.insert(ctx, ret);
        b.state.wasm.append_operation(ctx, ramp.get_operation(), 0);
    }

    // Step: the body, entered through a dispatch on the frame's state.
    let step_ty = FunctionType::get(ctx, vec![i32], vec![]);
    let step = FuncOp::new(ctx, step_name(&name), step_ty);
    let dispatch = step.get_entry_block(ctx);
    let frame = dispatch.deref(ctx).get_argument(0);
    let results = mir_results
        .iter()
        .map(|t| wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    let mut b = FunctionLowering::glue(state, ctx, &step);
    b.throws = true;
    b.results = results;
    b.frame = Some(Frame {
        frame,
        slots,
        resumes: Vec::new(),
    });
    let start = b.map_blocks(ctx, mir, None)?;
    b.translate(ctx, mir)?;

    // Dispatch: state 0 starts with the parameters from the frame; state k resumes await k.
    b.block = dispatch;
    let current = b.load(ctx, MemKind::I32, frame, STATE);
    let resumes = b.frame.as_ref().unwrap().resumes.clone();
    let mut targets = vec![start];
    for take in &resumes {
        let resume = b.new_block(ctx, vec![]);
        let saved = std::mem::replace(&mut b.block, resume);
        let awaited = b.load(ctx, MemKind::I32, frame, AWAITED);
        b.br(ctx, *take, vec![awaited]);
        b.block = saved;
        targets.push(resume);
    }
    for (index, target) in targets.iter().enumerate() {
        let args = if index == 0 {
            let mut loaded = Vec::new();
            for param in &params {
                let (at, access) = b.frame.as_ref().unwrap().slots[param];
                loaded.push(b.load(ctx, access, frame, at));
            }
            loaded
        } else {
            vec![]
        };
        let expected = b.i32_const(ctx, index as i32);
        let matches = b.num(ctx, NumOpcode::I32Eq, vec![current, expected]);
        let next = b.new_block(ctx, vec![]);
        b.cond_br(ctx, matches, (*target, args), (next, vec![]));
        b.block = next;
    }
    let unreachable = UnreachableOp::new(ctx).get_operation();
    b.insert(ctx, unreachable);
    b.state.wasm.append_operation(ctx, step.get_operation(), 0);
    Ok(())
}

/// `drive()`: run queued frames of the current task until none is ready.
pub(crate) fn drive(ctx: &mut Context, state: &mut ModuleState, kinds: &[Identifier]) {
    let ty = FunctionType::get(ctx, vec![], vec![]);
    let function = FuncOp::new(ctx, drive_name(), ty);
    let mut b = FunctionLowering::glue(state, ctx, &function);
    let header = b.new_block(ctx, vec![]);
    b.br(ctx, header, vec![]);
    b.block = header;
    let frame = b.runtime(ctx, Runtime::ReadyPop, vec![]).expect("frame");
    let idle = b.num(ctx, NumOpcode::I32Eqz, vec![frame]);
    let done = b.new_block(ctx, vec![]);
    let run = b.new_block(ctx, vec![]);
    b.cond_br(ctx, idle, (done, vec![]), (run, vec![]));
    b.block = done;
    let ret = ReturnOp::new(ctx, vec![]).get_operation();
    b.insert(ctx, ret);
    b.block = run;
    let kind = b.load(ctx, MemKind::I32, frame, KIND);
    for (index, name) in kinds.iter().enumerate() {
        let expected = b.i32_const(ctx, index as i32);
        let matches = b.num(ctx, NumOpcode::I32Eq, vec![kind, expected]);
        let call_block = b.new_block(ctx, vec![]);
        let next = b.new_block(ctx, vec![]);
        b.cond_br(ctx, matches, (call_block, vec![]), (next, vec![]));
        b.block = call_block;
        let call = CallOp::new(ctx, step_name(name), vec![frame], vec![]).get_operation();
        b.insert(ctx, call);
        b.br(ctx, header, vec![]);
        b.block = next;
    }
    let unreachable = UnreachableOp::new(ctx).get_operation();
    b.insert(ctx, unreachable);
    b.state
        .wasm
        .append_operation(ctx, function.get_operation(), 0);
}

impl FunctionLowering<'_> {
    /// The 8-byte slot bits of a value, as promises store it.
    pub(crate) fn slot_bits(&mut self, ctx: &mut Context, value: Value) -> Value {
        if value.get_type(ctx).deref(ctx).is::<FP64Type>() {
            self.num(ctx, NumOpcode::I64ReinterpretF64, vec![value])
        } else {
            self.num(ctx, NumOpcode::I64ExtendI32U, vec![value])
        }
    }

    /// `await promise` inside a step function.
    pub(crate) fn lower_await(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        let Some(frame) = self.frame.as_ref().map(|f| f.frame) else {
            anyhow::bail!("await outside an async function");
        };
        let promise = self.operand(ctx, op, 0)?;
        self.runtime(ctx, Runtime::PromiseObserve, vec![promise]);
        let i32 = i32_type(ctx);
        let take = self.new_block(ctx, vec![i32]);
        let suspend = self.new_block(ctx, vec![]);
        let settled = self.load(ctx, MemKind::I32, promise, promises::STATE);
        self.cond_br(ctx, settled, (take, vec![promise]), (suspend, vec![]));

        let resumes = &mut self.frame.as_mut().unwrap().resumes;
        resumes.push(take);
        let state = resumes.len() as i32;
        self.block = suspend;
        let state = self.i32_const(ctx, state);
        self.store(ctx, MemKind::I32, frame, STATE, state);
        self.store(ctx, MemKind::I32, frame, AWAITED, promise);
        self.runtime(ctx, Runtime::PromiseWait, vec![promise, frame]);
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        self.insert(ctx, ret);

        self.block = take;
        let promise = take.deref(ctx).get_argument(0);
        let settled = self.load(ctx, MemKind::I32, promise, promises::STATE);
        let rejected_state = self.i32_const(ctx, promises::REJECTED as i32);
        let rejected = self.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
        let rejection = self.new_block(ctx, vec![]);
        let fulfilled = self.new_block(ctx, vec![]);
        self.cond_br(ctx, rejected, (rejection, vec![]), (fulfilled, vec![]));
        self.block = rejection;
        let status = self
            .runtime(ctx, Runtime::PromiseRethrow, vec![promise])
            .expect("status");
        let (target, args) = self.exception_target(ctx, status);
        self.br(ctx, target, args);
        self.block = fulfilled;
        let ty = op.deref(ctx).get_result(0).get_type(ctx);
        let value = self.load(ctx, slot_kind(ctx, ty)?, promise, promises::VALUE);
        self.define(ctx, op, value);
        Ok(())
    }

    /// `return value` in a step function: fulfil the frame's promise.
    pub(crate) fn settle_return(&mut self, ctx: &mut Context, values: Vec<Value>) {
        let frame = self.frame.as_ref().unwrap().frame;
        let promise = self.load(ctx, MemKind::I32, frame, PROMISE);
        let bits = match values.first() {
            Some(value) => self.slot_bits(ctx, *value),
            None => self.i64_const(ctx, 0),
        };
        self.runtime(ctx, Runtime::PromiseResolve, vec![promise, bits]);
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        self.insert(ctx, ret);
    }

    /// The throw target of a step function: reject the frame's promise.
    pub(crate) fn reject_block(&mut self, ctx: &mut Context) -> Ptr<BasicBlock> {
        let frame = self.frame.as_ref().unwrap().frame;
        let i32 = i32_type(ctx);
        let block = self.new_block(ctx, vec![i32]);
        let saved = std::mem::replace(&mut self.block, block);
        let promise = self.load(ctx, MemKind::I32, frame, PROMISE);
        let status = block.deref(ctx).get_argument(0);
        self.runtime(ctx, Runtime::PromiseReject, vec![promise, status]);
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        self.insert(ctx, ret);
        self.block = saved;
        block
    }

    /// `setTimeout(delay, value)`: an async-lowered `wait-for` whose completion settles the
    /// promise with `value`.
    pub(crate) fn lower_timer(&mut self, ctx: &mut Context, op: Ptr<Operation>) -> Result<()> {
        anyhow::ensure!(
            self.state.can_import(crate::MONOTONIC_CLOCK),
            "setTimeout needs the WIT world to import {}",
            crate::MONOTONIC_CLOCK
        );
        let delay = self.operand(ctx, op, 0)?;
        let value = self.operand(ctx, op, 1)?;
        // Node waits at least 1 ms; NaN and smaller delays become 1 ms.
        let one = self.f64_const(ctx, 1.0);
        let at_least = self.num(ctx, NumOpcode::F64Ge, vec![delay, one]);
        let delay = self.select(ctx, at_least, delay, one);
        let million = self.f64_const(ctx, 1e6);
        let nanos = self.num(ctx, NumOpcode::F64Mul, vec![delay, million]);
        let nanos = self.num(ctx, NumOpcode::I64TruncSatF64S, vec![nanos]);
        let status = self
            .host(ctx, crate::Host::WaitFor, vec![nanos])
            .expect("status");
        let promise = self
            .runtime(ctx, Runtime::PromiseNew, vec![])
            .expect("promise");
        let bits = self.slot_bits(ctx, value);
        let mask = self.i32_const(ctx, 0xF);
        let subtask_state = self.num(ctx, NumOpcode::I32And, vec![status, mask]);
        let returned_state = self.i32_const(ctx, 2);
        let returned = self.num(ctx, NumOpcode::I32Eq, vec![subtask_state, returned_state]);
        let now = self.new_block(ctx, vec![]);
        let later = self.new_block(ctx, vec![]);
        let joined = self.new_block(ctx, vec![]);
        self.cond_br(ctx, returned, (now, vec![]), (later, vec![]));
        self.block = now;
        self.runtime(ctx, Runtime::PromiseResolve, vec![promise, bits]);
        self.br(ctx, joined, vec![]);
        self.block = later;
        let four = self.i32_const(ctx, 4);
        let handle = self.num(ctx, NumOpcode::I32ShrU, vec![status, four]);
        self.runtime(ctx, Runtime::SubtaskRegister, vec![handle, promise, bits]);
        let set = self.waitable_set(ctx);
        self.host(ctx, crate::Host::WaitableJoin, vec![handle, set]);
        self.br(ctx, joined, vec![]);
        self.block = joined;
        self.define(ctx, op, promise);
        Ok(())
    }

    /// The current task's waitable set, created on first use.
    pub(crate) fn waitable_set(&mut self, ctx: &mut Context) -> Value {
        let i32 = i32_type(ctx);
        let existing = self.runtime(ctx, Runtime::TaskSet, vec![]).expect("set");
        let missing = self.num(ctx, NumOpcode::I32Eqz, vec![existing]);
        let create = self.new_block(ctx, vec![]);
        let have = self.new_block(ctx, vec![i32]);
        self.cond_br(ctx, missing, (create, vec![]), (have, vec![existing]));
        self.block = create;
        let set = self
            .host(ctx, crate::Host::WaitableSetNew, vec![])
            .expect("set");
        self.runtime(ctx, Runtime::TaskSetSet, vec![set]);
        self.br(ctx, have, vec![set]);
        self.block = have;
        have.deref(ctx).get_argument(0)
    }
}
