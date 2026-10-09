//! Component exports: canonical-ABI glue between WIT values and MIR values (`TODOs.md` P6.1).
//!
//! For each WIT export, a core function named after the export lifts the flat arguments into
//! MIR values, calls the TypeScript implementation and lowers its result, flat when it is one
//! core value, otherwise into a return area. `cabi_post_<export>` ends the task after the host
//! has lifted the result, so lowered strings and lists can point into the task region without
//! copies. Strings are UTF-8 on both sides and are passed through without transcoding.
//!
//! Supported: `bool`, integers, `f32`, `f64` (all ECMAScript numbers), `string`, `list<T>` and
//! records. An uncaught exception in an export without an error result traps; `result` types
//! follow with P6.2.
//!
//! Async exports (P3.8) use the callback ABI: `[async-lift]<export>` lifts the arguments,
//! starts the async function and returns through `finish`, which drives ready frames and either
//! delivers the settled value with `task.return` (code `EXIT`) or suspends the task and asks the
//! host to `WAIT` on its waitable set. `[callback][async-lift]<export>` resumes the task
//! (kept in `context.get 0`), settles the promise of a returned host subtask and finishes again.
use crate::{FunctionLowering, ModuleState, Runtime, i32_type};
use anyhow::{Result, anyhow, bail, ensure};
use perry_mir::types::{ArrayType, RecordType};
use perry_runtime::{array as layout, string as strings};
use pliron::{
    builtin::{
        op_interfaces::{SingleBlockRegionInterface, SymbolOpInterface},
        ops::FuncOp,
        type_interfaces::FunctionTypeInterface,
        types::{FP32Type, FP64Type, FunctionType, IntegerType, Signedness},
    },
    context::Context,
    identifier::Identifier,
    op::Op,
    r#type::{TypeHandle, TypedHandle},
    value::Value,
};
use pliron_wasm::{
    attrs::{MemKind, NumOpcode},
    control::{ReturnOp, UnreachableOp},
    ops::{CallOp, set_export},
};

/// A WIT value type as the canonical ABI sees it.
#[derive(Clone, Debug, PartialEq)]
pub enum AbiType {
    Bool,
    S8,
    U8,
    S16,
    U16,
    S32,
    U32,
    S64,
    U64,
    F32,
    F64,
    String,
    List(Box<AbiType>),
    /// Fields in WIT order, named as the TypeScript record fields.
    Record(Vec<(String, AbiType)>),
}

/// One component export bound to a TypeScript function.
#[derive(Clone, Debug)]
pub struct Export {
    /// Core export name: the WIT function name, or `interface#function`.
    pub core_name: String,
    /// The MIR function implementing it.
    pub function: String,
    pub params: Vec<AbiType>,
    pub result: Option<AbiType>,
    /// For an async export (lifted with the callback ABI): the import module and WIT function
    /// name of its `task.return`.
    pub task_return: Option<(String, String)>,
    /// `wasi:cli/run`'s `run: async func() -> result`, implemented by the top-level
    /// statements: the result is `err` after an uncaught exception or a non-zero exit code.
    pub command: bool,
}

const MAX_FLAT_PARAMS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Flat {
    I32,
    I64,
    F32,
    F64,
}

impl AbiType {
    fn flatten(&self, out: &mut Vec<Flat>) {
        match self {
            AbiType::Bool
            | AbiType::S8
            | AbiType::U8
            | AbiType::S16
            | AbiType::U16
            | AbiType::S32
            | AbiType::U32 => out.push(Flat::I32),
            AbiType::S64 | AbiType::U64 => out.push(Flat::I64),
            AbiType::F32 => out.push(Flat::F32),
            AbiType::F64 => out.push(Flat::F64),
            AbiType::String | AbiType::List(_) => out.extend([Flat::I32, Flat::I32]),
            AbiType::Record(fields) => fields.iter().for_each(|(_, ty)| ty.flatten(out)),
        }
    }

    fn flat(&self) -> Vec<Flat> {
        let mut out = Vec::new();
        self.flatten(&mut out);
        out
    }

    fn alignment(&self) -> u32 {
        match self {
            AbiType::Bool | AbiType::S8 | AbiType::U8 => 1,
            AbiType::S16 | AbiType::U16 => 2,
            AbiType::S32 | AbiType::U32 | AbiType::F32 => 4,
            AbiType::S64 | AbiType::U64 | AbiType::F64 => 8,
            AbiType::String | AbiType::List(_) => 4,
            AbiType::Record(fields) => fields.iter().map(|(_, t)| t.alignment()).max().unwrap_or(1),
        }
    }

    fn size(&self) -> u32 {
        match self {
            AbiType::Bool | AbiType::S8 | AbiType::U8 => 1,
            AbiType::S16 | AbiType::U16 => 2,
            AbiType::S32 | AbiType::U32 | AbiType::F32 => 4,
            AbiType::S64 | AbiType::U64 | AbiType::F64 => 8,
            AbiType::String | AbiType::List(_) => 8,
            AbiType::Record(_) => {
                let end = self.field_offsets().last().copied().unwrap_or(0);
                let last = match self {
                    AbiType::Record(fields) => fields.last().map_or(0, |(_, t)| t.size()),
                    _ => unreachable!(),
                };
                align(end + last, self.alignment())
            }
        }
    }

    fn field_offsets(&self) -> Vec<u32> {
        let AbiType::Record(fields) = self else {
            return vec![];
        };
        let mut offset = 0;
        fields
            .iter()
            .map(|(_, ty)| {
                offset = align(offset, ty.alignment());
                let at = offset;
                offset += ty.size();
                at
            })
            .collect()
    }

    /// The memory access for a scalar, or `None` for compound types.
    fn mem_kind(&self) -> Option<MemKind> {
        Some(match self {
            AbiType::Bool | AbiType::U8 => MemKind::I32U8,
            AbiType::S8 => MemKind::I32S8,
            AbiType::U16 => MemKind::I32U16,
            AbiType::S16 => MemKind::I32S16,
            AbiType::S32 | AbiType::U32 => MemKind::I32,
            AbiType::S64 | AbiType::U64 => MemKind::I64,
            AbiType::F32 => MemKind::F32,
            AbiType::F64 => MemKind::F64,
            _ => return None,
        })
    }
}

fn align(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

pub(crate) fn flat_type(ctx: &mut Context, flat: Flat) -> TypeHandle {
    match flat {
        Flat::I32 => i32_type(ctx),
        Flat::I64 => IntegerType::get(ctx, 64, Signedness::Signless).into(),
        Flat::F32 => FP32Type::get(ctx).into(),
        Flat::F64 => FP64Type::get(ctx).into(),
    }
}

fn identifier(name: &str) -> Identifier {
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("wit_{sanitized}").try_into().expect("identifier")
}

/// Emit the export function and its post-return function for `export`.
pub(crate) fn export(
    ctx: &mut Context,
    state: &mut ModuleState,
    function: &FuncOp,
    export: &Export,
) -> Result<()> {
    let is_async = state
        .async_functions
        .contains(&function.get_symbol_name(ctx));
    ensure!(
        is_async == export.task_return.is_some(),
        "{} is {}async but the WIT export is {}async",
        export.function,
        if is_async { "" } else { "not " },
        if export.task_return.is_some() {
            ""
        } else {
            "not "
        },
    );
    if is_async {
        return async_export(ctx, state, function, export);
    }
    let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    ensure!(
        mir_params.len() == export.params.len(),
        "{} takes {} parameters but the WIT export has {}",
        export.function,
        mir_params.len(),
        export.params.len()
    );
    ensure!(
        mir_results.len() == usize::from(export.result.is_some()),
        "{} returns {} values but the WIT export returns {}",
        export.function,
        mir_results.len(),
        usize::from(export.result.is_some())
    );
    let flat_params: Vec<Flat> = export.params.iter().flat_map(AbiType::flat).collect();
    ensure!(
        flat_params.len() <= MAX_FLAT_PARAMS,
        "exports with more than {MAX_FLAT_PARAMS} flat parameters are not supported yet"
    );
    let flat_result = export
        .result
        .as_ref()
        .map(AbiType::flat)
        .unwrap_or_default();
    let core_results: Vec<Flat> = if flat_result.len() > 1 {
        vec![Flat::I32]
    } else {
        flat_result.clone()
    };

    let params: Vec<_> = flat_params.iter().map(|f| flat_type(ctx, *f)).collect();
    let results: Vec<_> = core_results.iter().map(|f| flat_type(ctx, *f)).collect();
    let fn_ty = FunctionType::get(ctx, params, results.clone());
    let glue = FuncOp::new(ctx, identifier(&export.core_name), fn_ty);
    set_export(ctx, &glue, &export.core_name);
    let entry = glue.get_entry_block(ctx);
    let mut flat: Vec<Value> = entry.deref(ctx).arguments().collect();
    flat.reverse();

    let mut b = FunctionLowering::glue(state, ctx, &glue);
    b.runtime(ctx, Runtime::TaskEnter, vec![]);
    let mut args = Vec::new();
    for (ty, mir_ty) in export.params.iter().zip(mir_params) {
        args.push(b.lift_flat(ctx, ty, mir_ty, &mut flat)?);
    }
    let throws = b.state.throws.contains(&function.get_symbol_name(ctx));
    let mut call_results = mir_results
        .iter()
        .map(|t| crate::wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    if throws {
        call_results.push(i32_type(ctx));
    }
    let call = CallOp::new(ctx, function.get_symbol_name(ctx), args, call_results).get_operation();
    b.insert(ctx, call);
    let values: Vec<Value> = call.deref(ctx).results().collect();
    if throws {
        // An uncaught exception without a WIT error type has no value to return.
        let trap = b.new_block(ctx, vec![]);
        let normal = b.new_block(ctx, vec![]);
        b.cond_br(
            ctx,
            *values.last().unwrap(),
            (trap, vec![]),
            (normal, vec![]),
        );
        b.block = trap;
        let unreachable = UnreachableOp::new(ctx).get_operation();
        b.insert(ctx, unreachable);
        b.block = normal;
    }
    let returned = match (&export.result, mir_results.first()) {
        (Some(ty), Some(mir_ty)) if flat_result.len() == 1 => {
            vec![b.lower_flat(ctx, ty, *mir_ty, values[0])?]
        }
        (Some(ty), Some(mir_ty)) => {
            let size = b.i32_const(ctx, ty.size() as i32);
            let area = b.runtime(ctx, Runtime::Alloc, vec![size]).expect("area");
            b.lower_store(ctx, ty, *mir_ty, values[0], area, 0)?;
            vec![area]
        }
        _ => vec![],
    };
    let ret = ReturnOp::new(ctx, returned).get_operation();
    b.insert(ctx, ret);
    let state = b.state;
    state.wasm.append_operation(ctx, glue.get_operation(), 0);

    // `cabi_post_<export>` receives the core results and ends the task.
    let post_ty = FunctionType::get(ctx, results, vec![]);
    let post_name = format!("cabi_post_{}", export.core_name);
    let post = FuncOp::new(ctx, identifier(&post_name), post_ty);
    set_export(ctx, &post, &post_name);
    let mut b = FunctionLowering::glue(state, ctx, &post);
    b.runtime(ctx, Runtime::TaskFree, vec![]);
    let ret = ReturnOp::new(ctx, vec![]).get_operation();
    b.insert(ctx, ret);
    b.state.wasm.append_operation(ctx, post.get_operation(), 0);
    Ok(())
}

/// Callback codes and events of the callback ABI.
const EXIT: i32 = 0;
const WAIT: i32 = 2;
const EVENT_SUBTASK: i32 = 1;

/// `[async-lift]<export>`, its callback and their shared `finish`.
fn async_export(
    ctx: &mut Context,
    state: &mut ModuleState,
    function: &FuncOp,
    export: &Export,
) -> Result<()> {
    let (module, wit_name) = export.task_return.clone().expect("async export");
    let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    ensure!(
        mir_params.len() == export.params.len(),
        "{} takes {} parameters but the WIT export has {}",
        export.function,
        mir_params.len(),
        export.params.len()
    );
    let flat_params: Vec<Flat> = export.params.iter().flat_map(AbiType::flat).collect();
    ensure!(
        flat_params.len() <= MAX_FLAT_PARAMS,
        "exports with more than {MAX_FLAT_PARAMS} flat parameters are not supported yet"
    );
    // `task.return` takes the result flat, or through memory beyond MAX_FLAT_PARAMS.
    let result_flat = if export.command {
        vec![Flat::I32]
    } else {
        export
            .result
            .as_ref()
            .map(AbiType::flat)
            .unwrap_or_default()
    };
    let indirect = result_flat.len() > MAX_FLAT_PARAMS;
    let task_return = crate::Host::TaskReturn(
        module,
        wit_name,
        if indirect {
            vec![Flat::I32]
        } else {
            result_flat
        },
    );
    let i32 = i32_type(ctx);

    // finish_<export>() -> code
    let finish_name = identifier(&format!("finish_{}", export.core_name));
    let finish_ty = FunctionType::get(ctx, vec![], vec![i32]);
    let finish = FuncOp::new(ctx, finish_name.clone(), finish_ty);
    {
        let mut b = FunctionLowering::glue(state, ctx, &finish);
        let uses_io = b.state.uses_io;
        let drive = CallOp::new(ctx, crate::async_split::drive_name(), vec![], vec![]);
        b.insert(ctx, drive.get_operation());
        if uses_io {
            b.runtime(ctx, Runtime::IoPump, vec![]);
        }
        let promise = b
            .runtime(ctx, Runtime::TaskResult, vec![])
            .expect("promise");
        let settled = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATE);
        let wait = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.cond_br(ctx, settled, (done, vec![]), (wait, vec![]));

        // Pending: leave the task and wait on its subtasks (none would wait forever).
        b.block = wait;
        let set = b.runtime(ctx, Runtime::TaskSuspend, vec![]).expect("set");
        let stuck = b.num(ctx, NumOpcode::I32Eqz, vec![set]);
        let trap = b.new_block(ctx, vec![]);
        let suspend = b.new_block(ctx, vec![]);
        b.cond_br(ctx, stuck, (trap, vec![]), (suspend, vec![]));
        b.block = trap;
        let unreachable = UnreachableOp::new(ctx).get_operation();
        b.insert(ctx, unreachable);
        b.block = suspend;
        let four = b.i32_const(ctx, 4);
        let shifted = b.num(ctx, NumOpcode::I32Shl, vec![set, four]);
        let wait_code = b.i32_const(ctx, WAIT);
        let code = b.num(ctx, NumOpcode::I32Or, vec![shifted, wait_code]);
        let ret = ReturnOp::new(ctx, vec![code]).get_operation();
        b.insert(ctx, ret);

        // Settled: a rejection without a WIT error type traps (a command reports it and
        // fails); otherwise return the value once the output reached the host.
        b.block = done;
        let outcome = if export.command {
            Some(
                b.runtime(ctx, Runtime::CommandOutcome, vec![promise])
                    .expect("outcome"),
            )
        } else {
            let rejected_state = b.i32_const(ctx, perry_runtime::promise::REJECTED as i32);
            let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
            let trap = b.new_block(ctx, vec![]);
            let fulfilled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, rejected, (trap, vec![]), (fulfilled, vec![]));
            b.block = trap;
            let unreachable = UnreachableOp::new(ctx).get_operation();
            b.insert(ctx, unreachable);
            b.block = fulfilled;
            None
        };
        if uses_io {
            let flushed = b.runtime(ctx, Runtime::IoClose, vec![]).expect("flushed");
            let closed = b.new_block(ctx, vec![]);
            b.cond_br(ctx, flushed, (closed, vec![]), (wait, vec![]));
            b.block = closed;
        }
        let mut flat = Vec::new();
        if let Some(outcome) = outcome {
            flat.push(outcome);
        } else if let (Some(ty), Some(mir_ty)) = (&export.result, mir_results.first()) {
            let kind = crate::slot_kind(ctx, *mir_ty)?;
            let value = b.load(ctx, kind, promise, perry_runtime::promise::VALUE);
            if indirect {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.runtime(ctx, Runtime::Alloc, vec![size]).expect("area");
                b.lower_store(ctx, ty, *mir_ty, value, area, 0)?;
                flat.push(area);
            } else {
                b.lower_flat_values(ctx, ty, *mir_ty, value, &mut flat)?;
            }
        }
        b.host(ctx, task_return, flat);
        b.runtime(ctx, Runtime::TaskCloseSet, vec![]);
        b.runtime(ctx, Runtime::TaskFree, vec![]);
        let exit = b.i32_const(ctx, EXIT);
        let ret = ReturnOp::new(ctx, vec![exit]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, finish.get_operation(), 0);
    }

    // [async-lift]<export>(params) -> code
    let lift_name = format!("[async-lift]{}", export.core_name);
    let params: Vec<_> = flat_params.iter().map(|f| flat_type(ctx, *f)).collect();
    let lift_ty = FunctionType::get(ctx, params, vec![i32]);
    let lift = FuncOp::new(ctx, identifier(&lift_name), lift_ty);
    set_export(ctx, &lift, &lift_name);
    {
        let mut flat: Vec<Value> = lift.get_entry_block(ctx).deref(ctx).arguments().collect();
        flat.reverse();
        let mut b = FunctionLowering::glue(state, ctx, &lift);
        let task = b.runtime(ctx, Runtime::TaskEnter, vec![]).expect("task");
        b.host(ctx, crate::Host::ContextSet0, vec![task]);
        let mut args = Vec::new();
        for (ty, mir_ty) in export.params.iter().zip(&mir_params) {
            args.push(b.lift_flat(ctx, ty, *mir_ty, &mut flat)?);
        }
        let ramp = CallOp::new(ctx, function.get_symbol_name(ctx), args, vec![i32]);
        let promise = b.result(ctx, ramp.get_operation());
        b.runtime(ctx, Runtime::TaskSetResult, vec![promise]);
        let call = CallOp::new(ctx, finish_name.clone(), vec![], vec![i32]);
        let code = b.result(ctx, call.get_operation());
        let ret = ReturnOp::new(ctx, vec![code]).get_operation();
        b.insert(ctx, ret);
        b.state.wasm.append_operation(ctx, lift.get_operation(), 0);
    }

    // [callback][async-lift]<export>(event, p1, p2) -> code
    let callback_name = format!("[callback]{lift_name}");
    let callback_ty = FunctionType::get(ctx, vec![i32; 3], vec![i32]);
    let callback = FuncOp::new(ctx, identifier(&callback_name), callback_ty);
    set_export(ctx, &callback, &callback_name);
    {
        let args: Vec<Value> = callback
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        let (event, handle, subtask_state) = (args[0], args[1], args[2]);
        let mut b = FunctionLowering::glue(state, ctx, &callback);
        let task = b.host(ctx, crate::Host::ContextGet0, vec![]).expect("task");
        b.runtime(ctx, Runtime::TaskResume, vec![task]);
        let next = b.new_block(ctx, vec![]);
        if b.state.uses_fs {
            // `node:fs` operations take their own subtask, stream and future events.
            let handled = b
                .call(
                    ctx,
                    "fs_event",
                    &[crate::W::I32; 3],
                    &[crate::W::I32],
                    vec![event, handle, subtask_state],
                )
                .expect("handled");
            let unhandled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, handled, (next, vec![]), (unhandled, vec![]));
            b.block = unhandled;
        }
        let subtask_event = b.i32_const(ctx, EVENT_SUBTASK);
        let is_subtask = b.num(ctx, NumOpcode::I32Eq, vec![event, subtask_event]);
        let settle = b.new_block(ctx, vec![]);
        let other = b.new_block(ctx, vec![]);
        b.cond_br(ctx, is_subtask, (settle, vec![]), (other, vec![]));
        b.block = settle;
        b.runtime(ctx, Runtime::TaskSubtaskEvent, vec![handle, subtask_state]);
        b.br(ctx, next, vec![]);
        b.block = other;
        if b.state.uses_io {
            b.runtime(ctx, Runtime::IoEvent, vec![event, handle, subtask_state]);
        }
        b.br(ctx, next, vec![]);
        b.block = next;
        let call = CallOp::new(ctx, finish_name, vec![], vec![i32]);
        let code = b.result(ctx, call.get_operation());
        let ret = ReturnOp::new(ctx, vec![code]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, callback.get_operation(), 0);
    }
    Ok(())
}

/// `cabi_realloc`, through which the host lowers strings and lists into guest memory.
pub(crate) fn realloc(ctx: &mut Context, state: &mut ModuleState) {
    let i32 = i32_type(ctx);
    let ty = FunctionType::get(ctx, vec![i32; 4], vec![i32]);
    let function = FuncOp::new(ctx, "cabi_realloc".try_into().unwrap(), ty);
    set_export(ctx, &function, "cabi_realloc");
    let args: Vec<_> = function
        .get_entry_block(ctx)
        .deref(ctx)
        .arguments()
        .collect();
    let mut b = FunctionLowering::glue(state, ctx, &function);
    let block = b.runtime(ctx, Runtime::CabiRealloc, args).expect("block");
    let ret = ReturnOp::new(ctx, vec![block]).get_operation();
    b.insert(ctx, ret);
    b.state
        .wasm
        .append_operation(ctx, function.get_operation(), 0);
}

fn expect_number(ctx: &Context, mir_ty: TypeHandle, ty: &AbiType) -> Result<()> {
    ensure!(
        mir_ty.deref(ctx).is::<FP64Type>(),
        "WIT {ty:?} needs a TypeScript number"
    );
    Ok(())
}

impl FunctionLowering<'_> {
    /// Convert a scalar core value to the MIR value of type `mir_ty`.
    fn lift_scalar(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        raw: Value,
    ) -> Result<Value> {
        let opcode = match ty {
            AbiType::Bool => {
                ensure!(
                    crate::repr(ctx, mir_ty)? == crate::Repr::Bool,
                    "WIT bool needs a TypeScript boolean"
                );
                let zero = self.i32_const(ctx, 0);
                return Ok(self.num(ctx, NumOpcode::I32Ne, vec![raw, zero]));
            }
            AbiType::S8 | AbiType::S16 | AbiType::S32 => NumOpcode::F64ConvertI32S,
            AbiType::U8 | AbiType::U16 | AbiType::U32 => NumOpcode::F64ConvertI32U,
            AbiType::S64 => NumOpcode::F64ConvertI64S,
            AbiType::U64 => NumOpcode::F64ConvertI64U,
            AbiType::F32 => NumOpcode::F64PromoteF32,
            AbiType::F64 => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(raw);
            }
            _ => bail!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![raw]))
    }

    /// Lift a value from flat core values (consumed from the end of `flat`).
    fn lift_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        flat: &mut Vec<Value>,
    ) -> Result<Value> {
        let mut next = || flat.pop().ok_or_else(|| anyhow!("too few flat values"));
        match ty {
            AbiType::String => {
                let (ptr, len) = (next()?, next()?);
                self.lift_string(ctx, mir_ty, ptr, len)
            }
            AbiType::List(element) => {
                let (ptr, len) = (next()?, next()?);
                self.lift_list(ctx, element, mir_ty, ptr, len)
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                let mut values = Vec::new();
                for (name, field_ty) in fields {
                    let (_, field_mir) = record_field(ctx, record, name)?;
                    values.push((
                        name.clone(),
                        self.lift_flat(ctx, field_ty, field_mir, flat)?,
                    ));
                }
                self.build_record(ctx, mir_ty, values)
            }
            scalar => {
                let raw = next()?;
                self.lift_scalar(ctx, scalar, mir_ty, raw)
            }
        }
    }

    /// Lift a value stored at `address + offset` in canonical layout.
    fn lift_load(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        address: Value,
        offset: u32,
    ) -> Result<Value> {
        match ty {
            AbiType::String => {
                let ptr = self.load(ctx, MemKind::I32, address, offset);
                let len = self.load(ctx, MemKind::I32, address, offset + 4);
                self.lift_string(ctx, mir_ty, ptr, len)
            }
            AbiType::List(element) => {
                let ptr = self.load(ctx, MemKind::I32, address, offset);
                let len = self.load(ctx, MemKind::I32, address, offset + 4);
                self.lift_list(ctx, element, mir_ty, ptr, len)
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                let mut values = Vec::new();
                for ((name, field_ty), at) in fields.iter().zip(ty.field_offsets()) {
                    let (_, field_mir) = record_field(ctx, record, name)?;
                    let value = self.lift_load(ctx, field_ty, field_mir, address, offset + at)?;
                    values.push((name.clone(), value));
                }
                self.build_record(ctx, mir_ty, values)
            }
            scalar => {
                let raw = self.load(ctx, scalar.mem_kind().unwrap(), address, offset);
                self.lift_scalar(ctx, scalar, mir_ty, raw)
            }
        }
    }

    fn lift_string(
        &mut self,
        ctx: &mut Context,
        mir_ty: TypeHandle,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        ensure!(
            mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
            "WIT string needs a TypeScript string"
        );
        Ok(self
            .runtime(ctx, Runtime::StringAdopt, vec![ptr, len])
            .expect("string"))
    }

    fn lift_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        let element_mir = TypedHandle::<ArrayType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT list needs a TypeScript array"))?
            .deref(ctx)
            .element();
        let kind = crate::slot_kind(ctx, element_mir)?;
        let array = self
            .runtime(ctx, Runtime::ArrayNew, vec![len])
            .expect("array");
        let data = self.load(ctx, MemKind::I32, array, layout::DATA);
        let size = element.size() as i32;
        self.counted_loop(ctx, len, |b, ctx, i| {
            let stride = b.i32_const(ctx, size);
            let offset = b.num(ctx, NumOpcode::I32Mul, vec![i, stride]);
            let source = b.num(ctx, NumOpcode::I32Add, vec![ptr, offset]);
            let value = b.lift_load(ctx, element, element_mir, source, 0)?;
            let slot = b.slot_address(ctx, data, i);
            b.store(ctx, kind, slot, 0, value);
            Ok(())
        })?;
        self.store(ctx, MemKind::I32, array, layout::LEN, len);
        Ok(array)
    }

    fn record_type(&self, ctx: &Context, mir_ty: TypeHandle) -> Result<TypedHandle<RecordType>> {
        TypedHandle::<RecordType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT record needs a TypeScript object type"))
    }

    /// Allocate a MIR record from named field values.
    fn build_record(
        &mut self,
        ctx: &mut Context,
        mir_ty: TypeHandle,
        values: Vec<(String, Value)>,
    ) -> Result<Value> {
        let record = self.record_type(ctx, mir_ty)?;
        let (name, field_count) = {
            let record = record.deref(ctx);
            let count = record
                .fields()
                .map_err(|_| anyhow!("record without fields"))?
                .len();
            (record.name().to_string(), count)
        };
        ensure!(
            values.len() == field_count,
            "the WIT record has {} fields but the TypeScript type {name} has {field_count}",
            values.len()
        );
        let next_shape = self.state.shapes.len() as i32 + 1;
        let shape = *self.state.shapes.entry(name).or_insert(next_shape);
        let size = self.i32_const(ctx, 8 + 8 * field_count as i32);
        let object = self
            .runtime(ctx, Runtime::Alloc, vec![size])
            .expect("record");
        let shape = self.i32_const(ctx, shape);
        self.store(ctx, MemKind::I32, object, 0, shape);
        for (field, value) in values {
            let (index, field_mir) = record_field(ctx, record, &field)?;
            let kind = crate::slot_kind(ctx, field_mir)?;
            self.store(ctx, kind, object, 8 + 8 * index as u32, value);
        }
        Ok(object)
    }

    /// Convert a MIR value to a scalar core value.
    fn lower_flat(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<Value> {
        let opcode = match ty {
            AbiType::Bool => return Ok(value),
            AbiType::S8 | AbiType::S16 | AbiType::S32 => NumOpcode::I32TruncSatF64S,
            AbiType::U8 | AbiType::U16 | AbiType::U32 => NumOpcode::I32TruncSatF64U,
            AbiType::S64 => NumOpcode::I64TruncSatF64S,
            AbiType::U64 => NumOpcode::I64TruncSatF64U,
            AbiType::F32 => NumOpcode::F32DemoteF64,
            AbiType::F64 => {
                expect_number(ctx, mir_ty, ty)?;
                return Ok(value);
            }
            _ => bail!("{ty:?} is not a scalar"),
        };
        expect_number(ctx, mir_ty, ty)?;
        Ok(self.num(ctx, opcode, vec![value]))
    }

    /// Store a MIR value at `address + offset` in canonical layout.
    fn lower_store(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
        address: Value,
        offset: u32,
    ) -> Result<()> {
        match ty {
            AbiType::String => {
                ensure!(
                    mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
                    "WIT string needs a TypeScript string"
                );
                let data = self.load(ctx, MemKind::I32, value, strings::DATA);
                let len = self.load(ctx, MemKind::I32, value, strings::LEN);
                self.store(ctx, MemKind::I32, address, offset, data);
                self.store(ctx, MemKind::I32, address, offset + 4, len);
            }
            AbiType::List(element) => {
                let (buffer, len) = self.lower_list(ctx, element, mir_ty, value)?;
                self.store(ctx, MemKind::I32, address, offset, buffer);
                self.store(ctx, MemKind::I32, address, offset + 4, len);
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                for ((name, field_ty), at) in fields.iter().zip(ty.field_offsets()) {
                    let (index, field_mir) = record_field(ctx, record, name)?;
                    let kind = crate::slot_kind(ctx, field_mir)?;
                    let field = self.load(ctx, kind, value, 8 + 8 * index as u32);
                    self.lower_store(ctx, field_ty, field_mir, field, address, offset + at)?;
                }
            }
            scalar => {
                let raw = self.lower_flat(ctx, scalar, mir_ty, value)?;
                self.store(ctx, scalar.mem_kind().unwrap(), address, offset, raw);
            }
        }
        Ok(())
    }

    /// Copy a MIR array into a canonical list buffer; returns (buffer, length).
    fn lower_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<(Value, Value)> {
        let element_mir = TypedHandle::<ArrayType>::from_handle(mir_ty, ctx)
            .map_err(|_| anyhow!("WIT list needs a TypeScript array"))?
            .deref(ctx)
            .element();
        let kind = crate::slot_kind(ctx, element_mir)?;
        let len = self.load(ctx, MemKind::I32, value, layout::LEN);
        let data = self.load(ctx, MemKind::I32, value, layout::DATA);
        let stride = self.i32_const(ctx, element.size() as i32);
        let bytes = self.num(ctx, NumOpcode::I32Mul, vec![len, stride]);
        let buffer = self
            .runtime(ctx, Runtime::Alloc, vec![bytes])
            .expect("list");
        let size = element.size() as i32;
        self.counted_loop(ctx, len, |b, ctx, i| {
            let slot = b.slot_address(ctx, data, i);
            let item = b.load(ctx, kind, slot, 0);
            let stride = b.i32_const(ctx, size);
            let at = b.num(ctx, NumOpcode::I32Mul, vec![i, stride]);
            let target = b.num(ctx, NumOpcode::I32Add, vec![buffer, at]);
            b.lower_store(ctx, element, element_mir, item, target, 0)
        })?;
        Ok((buffer, len))
    }

    /// Lower a MIR value to its flat core values (appended to `out`).
    fn lower_flat_values(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
        out: &mut Vec<Value>,
    ) -> Result<()> {
        match ty {
            AbiType::String => {
                ensure!(
                    mir_ty.deref(ctx).is::<perry_mir::types::StringType>(),
                    "WIT string needs a TypeScript string"
                );
                out.push(self.load(ctx, MemKind::I32, value, strings::DATA));
                out.push(self.load(ctx, MemKind::I32, value, strings::LEN));
            }
            AbiType::List(element) => {
                let (buffer, len) = self.lower_list(ctx, element, mir_ty, value)?;
                out.extend([buffer, len]);
            }
            AbiType::Record(fields) => {
                let record = self.record_type(ctx, mir_ty)?;
                for (name, field_ty) in fields {
                    let (index, field_mir) = record_field(ctx, record, name)?;
                    let kind = crate::slot_kind(ctx, field_mir)?;
                    let field = self.load(ctx, kind, value, 8 + 8 * index as u32);
                    self.lower_flat_values(ctx, field_ty, field_mir, field, out)?;
                }
            }
            scalar => out.push(self.lower_flat(ctx, scalar, mir_ty, value)?),
        }
        Ok(())
    }

    /// `for (i = 0; i < count; i++) body(i)` as a CFG; code after it continues in the exit.
    pub(crate) fn counted_loop(
        &mut self,
        ctx: &mut Context,
        count: Value,
        body: impl FnOnce(&mut Self, &mut Context, Value) -> Result<()>,
    ) -> Result<()> {
        let i32 = i32_type(ctx);
        let header = self.new_block(ctx, vec![i32]);
        let inside = self.new_block(ctx, vec![]);
        let exit = self.new_block(ctx, vec![]);
        let zero = self.i32_const(ctx, 0);
        self.br(ctx, header, vec![zero]);
        self.block = header;
        let i = header.deref(ctx).get_argument(0);
        let more = self.num(ctx, NumOpcode::I32LtU, vec![i, count]);
        self.cond_br(ctx, more, (inside, vec![]), (exit, vec![]));
        self.block = inside;
        body(self, ctx, i)?;
        let one = self.i32_const(ctx, 1);
        let next = self.num(ctx, NumOpcode::I32Add, vec![i, one]);
        self.br(ctx, header, vec![next]);
        self.block = exit;
        Ok(())
    }
}

/// Slot index and MIR type of the TypeScript field `name`.
fn record_field(
    ctx: &Context,
    record: TypedHandle<RecordType>,
    name: &str,
) -> Result<(usize, TypeHandle)> {
    let field: Identifier = name
        .try_into()
        .map_err(|_| anyhow!("field name {name} is not an identifier"))?;
    let record = record.deref(ctx);
    record
        .field(&field)
        .ok_or_else(|| anyhow!("TypeScript type {} has no field {name}", record.name()))
}
