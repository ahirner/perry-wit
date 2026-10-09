//! Component exports: canonical-ABI glue between WIT values and MIR values (`TODOs.md` P6.1).
//!
//! For each WIT export, a core function named after the export lifts the flat arguments into
//! MIR values, calls the TypeScript implementation and lowers its result, flat when it is one
//! core value, otherwise into a return area. `cabi_post_<export>` ends the task after the host
//! has lifted the result, so lowered strings and lists can point into the task region without
//! copies. Strings are UTF-8 on both sides and are passed through without transcoding.
//!
//! Supported: `bool`, integers, `f32`, `f64` (all ECMAScript numbers), `string`, `list<T>` and
//! records, and closed string/byte-list variants. Top-level `result` exports adapt returns
//! and throws to primitive or Error-record cases. Other uncaught exceptions trap.
//!
//! Async exports (P3.8) use the callback ABI: `[async-lift]<export>` lifts the arguments,
//! invokes the implementation and returns through `finish`, which drives ready frames and either
//! delivers the settled value with `task.return` (code `EXIT`) or suspends the task and asks the
//! host to `WAIT` on its waitable set. `[callback][async-lift]<export>` resumes the task
//! (kept in `context.get 0`), settles the promise of a returned host subtask and finishes again.
mod completions;
use completions::{Completion, EntryResult};

use crate::{FunctionLowering, ModuleState, Runtime, i32_type};
use anyhow::{Result, anyhow, bail, ensure};
use perry_mir::types::{ArrayType, BytesType, RecordType, StringType, TextOrBytesType};
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
    TextOrBytes(TextBytesOrder),
    Result {
        ok: Option<Box<AbiType>>,
        err: Option<Box<AbiType>>,
    },
    /// Fields in WIT order, named as the TypeScript record fields.
    Record(Vec<(String, AbiType)>),
}

/// The discriminant of the string case in a two-case string/byte-list variant.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(i32)]
pub enum TextBytesOrder {
    TextFirst = 0,
    BytesFirst = 1,
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
    pub kind: ExportKind,
}

/// The completion protocol; async variants name the `task.return` import.
#[derive(Clone, Debug)]
pub enum ExportKind {
    Sync,
    Async {
        module: String,
        function: String,
    },
    /// Top-level execution: fulfilment is `ok`, an uncaught exception is `err`.
    Command {
        module: String,
        function: String,
    },
}

const MAX_FLAT_PARAMS: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Flat {
    I32,
    I64,
    F32,
    F64,
}

impl Flat {
    fn join(self, other: Self) -> Self {
        if self == other {
            self
        } else if matches!(
            (self, other),
            (Self::I32 | Self::F32, Self::I32 | Self::F32)
        ) {
            Self::I32
        } else {
            Self::I64
        }
    }
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
            AbiType::TextOrBytes(_) => out.extend([Flat::I32; 3]),
            AbiType::Result { ok, err } => {
                out.push(Flat::I32);
                let ok = ok.as_ref().map_or_else(Vec::new, |ty| ty.flat());
                let err = err.as_ref().map_or_else(Vec::new, |ty| ty.flat());
                for i in 0..ok.len().max(err.len()) {
                    out.push(match (ok.get(i), err.get(i)) {
                        (Some(a), Some(b)) => a.join(*b),
                        (Some(a), None) | (None, Some(a)) => *a,
                        _ => unreachable!(),
                    });
                }
            }
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
            AbiType::String | AbiType::List(_) | AbiType::TextOrBytes(_) => 4,
            AbiType::Result { ok, err } => ok
                .iter()
                .chain(err)
                .map(|ty| ty.alignment())
                .max()
                .unwrap_or(1),
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
            AbiType::TextOrBytes(_) => 12,
            AbiType::Result { ok, err } => {
                let payload = ok.iter().chain(err).map(|ty| ty.size()).max().unwrap_or(0);
                align(self.alignment() + payload, self.alignment())
            }
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
    if !matches!(export.kind, ExportKind::Sync) {
        return async_export(ctx, state, function, export);
    }
    ensure!(
        !state.uses_filesystem,
        "filesystem operations require async WIT exports"
    );
    ensure!(
        !state.uses_output(Runtime::Stdout) && !state.uses_output(Runtime::Stderr),
        "console output requires async WIT exports to drain P3 streams"
    );
    let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let entry_result = EntryResult::new(ctx, state, function, &mir_results)?;
    ensure!(
        mir_params.len() == export.params.len(),
        "{} takes {} parameters but the WIT export has {}",
        export.function,
        mir_params.len(),
        export.params.len()
    );
    let has_value = match &export.result {
        Some(AbiType::Result { ok, .. }) => ok.is_some(),
        other => other.is_some(),
    };
    ensure!(
        !has_value || entry_result.settled_type().is_some(),
        "{} must return a value for this WIT export",
        export.function
    );
    let mut flat_params: Vec<Flat> = export.params.iter().flat_map(AbiType::flat).collect();
    let indirect_params = flat_params.len() > MAX_FLAT_PARAMS;
    if indirect_params {
        flat_params = vec![Flat::I32];
    }
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
    let args = b.lift_arguments(ctx, &export.params, &mir_params, flat, indirect_params)?;
    let completion = b.invoke_export(ctx, function, args, entry_result)?;
    if let Completion::Promise { promise, .. } = completion {
        b.runtime(ctx, Runtime::PromiseObserve, vec![promise]);
        b.finish_sync(ctx);
        let state = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATE);
        let pending = b.num(ctx, NumOpcode::I32Eqz, vec![state]);
        b.abort_if(ctx, pending);
    }
    if !matches!(export.result, Some(AbiType::Result { .. })) {
        let status = match completion {
            Completion::Call { status, .. } => status,
            Completion::Promise { promise, .. } => {
                b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATUS)
            }
        };
        if !matches!(entry_result, EntryResult::Direct { throws: false, .. }) {
            b.abort_if(ctx, status);
        }
        if matches!(completion, Completion::Call { .. }) {
            b.finish_sync(ctx);
        }
    }
    let returned = match &export.result {
        Some(ty @ AbiType::Result { .. }) => {
            b.lower_completion(ctx, ty, completion, flat_result.len() > 1)?
        }
        Some(ty) => {
            let (mir_ty, value) = match completion {
                Completion::Call { result, .. } => result.expect("checked export result"),
                Completion::Promise { result, promise } => {
                    let mir_ty = result.expect("checked settled type");
                    let kind = crate::slot_kind(ctx, mir_ty)?;
                    (
                        mir_ty,
                        b.load(ctx, kind, promise, perry_runtime::promise::VALUE),
                    )
                }
            };
            if flat_result.len() <= 1 {
                let mut flat = Vec::with_capacity(flat_result.len());
                b.lower_flat_values(ctx, ty, mir_ty, value, &mut flat)?;
                flat
            } else {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.runtime(ctx, Runtime::Alloc, vec![size]).expect("area");
                b.lower_store(ctx, ty, mir_ty, value, area, 0)?;
                vec![area]
            }
        }
        None => vec![],
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
const SUBTASK_RETURNED: i32 = 2;

/// `[async-lift]<export>`, its callback and their shared `finish`.
fn async_export(
    ctx: &mut Context,
    state: &mut ModuleState,
    function: &FuncOp,
    export: &Export,
) -> Result<()> {
    let (module, wit_name) = match &export.kind {
        ExportKind::Async { module, function } | ExportKind::Command { module, function } => {
            (module.clone(), function.clone())
        }
        ExportKind::Sync => unreachable!("async export"),
    };
    let command = matches!(export.kind, ExportKind::Command { .. });
    let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let entry_result = EntryResult::new(ctx, state, function, &mir_results)?;
    let settled_type = entry_result.settled_type();
    let has_value = match &export.result {
        Some(AbiType::Result { ok, .. }) => ok.is_some(),
        other => other.is_some(),
    };
    ensure!(
        !has_value || settled_type.is_some(),
        "{} must return a value for this WIT export",
        export.function
    );
    ensure!(
        mir_params.len() == export.params.len(),
        "{} takes {} parameters but the WIT export has {}",
        export.function,
        mir_params.len(),
        export.params.len()
    );
    let mut flat_params: Vec<Flat> = export.params.iter().flat_map(AbiType::flat).collect();
    let indirect_params = flat_params.len() > MAX_FLAT_PARAMS;
    if indirect_params {
        flat_params = vec![Flat::I32];
    }
    // `task.return` takes the result flat, or through memory beyond MAX_FLAT_PARAMS.
    let result_flat = if command {
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
        if !b.state.async_functions.is_empty() {
            let drive = CallOp::new(ctx, crate::async_split::drive_name(), vec![], vec![]);
            b.insert(ctx, drive.get_operation());
        }
        let failed = b
            .runtime(ctx, Runtime::TaskFailed, vec![])
            .expect("task failure");
        let abort = b.new_block(ctx, vec![]);
        let active = b.new_block(ctx, vec![]);
        b.cond_br(ctx, failed, (abort, vec![]), (active, vec![]));
        b.block = abort;
        b.abort_task(ctx);
        if command {
            let error = b.i32_const(ctx, 1);
            b.host(ctx, task_return.clone(), vec![error]);
            let exit = b.i32_const(ctx, EXIT);
            let ret = ReturnOp::new(ctx, vec![exit]).get_operation();
            b.insert(ctx, ret);
        } else {
            let unreachable = UnreachableOp::new(ctx).get_operation();
            b.insert(ctx, unreachable);
        }
        b.block = active;
        let promise = b
            .runtime(ctx, Runtime::TaskResult, vec![])
            .expect("promise");
        let settled = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::STATE);
        let pending = b
            .runtime(ctx, Runtime::TaskPending, vec![])
            .expect("pending host work");
        let drained = b.num(ctx, NumOpcode::I32Eqz, vec![pending]);
        let ready = b.num(ctx, NumOpcode::I32Eqz, vec![settled]);
        let ready = b.num(ctx, NumOpcode::I32Eqz, vec![ready]);
        let complete = b.num(ctx, NumOpcode::I32And, vec![ready, drained]);
        let wait = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.cond_br(ctx, complete, (done, vec![]), (wait, vec![]));

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

        // Settled: a rejection without a WIT error type traps; otherwise return the value.
        b.block = done;
        let rejected_state = b.i32_const(ctx, perry_runtime::promise::REJECTED as i32);
        let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
        let rejected = if command {
            ensure!(
                settled_type
                    .is_some_and(|ty| crate::repr(ctx, ty).is_ok_and(|ty| ty == crate::Repr::Bool)),
                "command entry must return its boolean ok result"
            );
            let ok = b.load(ctx, MemKind::I32, promise, perry_runtime::promise::VALUE);
            let error_result = b.num(ctx, NumOpcode::I32Eqz, vec![ok]);
            b.num(ctx, NumOpcode::I32Or, vec![rejected, error_result])
        } else {
            rejected
        };
        if command && b.state.uses_process {
            let status = b
                .runtime(ctx, Runtime::ExitStatus, vec![])
                .expect("exit status");
            let zero = b.i32_const(ctx, 0);
            let assigned = b.num(ctx, NumOpcode::I32GeS, vec![status, zero]);
            let success = b.num(ctx, NumOpcode::I32Eqz, vec![rejected]);
            let explicit = b.num(ctx, NumOpcode::I32And, vec![assigned, success]);
            let exit = b.new_block(ctx, vec![]);
            let next = b.new_block(ctx, vec![]);
            b.cond_br(ctx, explicit, (exit, vec![]), (next, vec![]));
            b.block = exit;
            b.exit_process(ctx);
            let unreachable = UnreachableOp::new(ctx).get_operation();
            b.insert(ctx, unreachable);
            b.block = next;
        }
        let completion_result = matches!(export.result, Some(AbiType::Result { .. }));
        let mut flat = if command {
            vec![rejected]
        } else if completion_result {
            Vec::new()
        } else {
            b.abort_if(ctx, rejected);
            Vec::new()
        };
        if let Some(ty @ AbiType::Result { .. }) = &export.result {
            flat = b.lower_completion(
                ctx,
                ty,
                Completion::Promise {
                    result: settled_type,
                    promise,
                },
                indirect,
            )?;
        } else if let (Some(ty), Some(mir_ty)) = (&export.result, settled_type) {
            let kind = crate::slot_kind(ctx, mir_ty)?;
            let value = b.load(ctx, kind, promise, perry_runtime::promise::VALUE);
            if indirect {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.runtime(ctx, Runtime::Alloc, vec![size]).expect("area");
                b.lower_store(ctx, ty, mir_ty, value, area, 0)?;
                flat.push(area);
            } else {
                b.lower_flat_values(ctx, ty, mir_ty, value, &mut flat)?;
            }
        }
        b.host(ctx, task_return, flat);
        let call = CallOp::new(ctx, identifier("release_task"), vec![], vec![]).get_operation();
        b.insert(ctx, call);
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
        let args = b.lift_arguments(ctx, &export.params, &mir_params, flat, indirect_params)?;
        let completion = b.invoke_export(ctx, function, args, entry_result)?;
        let promise = b.completion_promise(ctx, completion);
        b.runtime(ctx, Runtime::TaskSetResult, vec![promise]);
        if matches!(export.result, Some(AbiType::Result { .. })) {
            b.runtime(ctx, Runtime::PromiseObserve, vec![promise]);
        }
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
        let cancelled_event = b.i32_const(ctx, 6);
        let is_cancelled = b.num(ctx, NumOpcode::I32Eq, vec![event, cancelled_event]);
        let cancelled = b.new_block(ctx, vec![]);
        let active = b.new_block(ctx, vec![]);
        b.cond_br(ctx, is_cancelled, (cancelled, vec![]), (active, vec![]));
        b.block = cancelled;
        b.abort_task(ctx);
        b.host(ctx, crate::Host::TaskCancel, vec![]);
        let exit = b.i32_const(ctx, EXIT);
        let ret = ReturnOp::new(ctx, vec![exit]).get_operation();
        b.insert(ctx, ret);
        b.block = active;
        for (write, event_handler) in [
            (Runtime::Stdout, Runtime::StdoutEvent),
            (Runtime::Stderr, Runtime::StderrEvent),
        ] {
            if b.state.uses_output(write) {
                b.runtime(ctx, event_handler, vec![event, handle, subtask_state]);
            }
        }
        if b.state.uses_filesystem {
            b.runtime(ctx, Runtime::FsEvent, vec![event, handle, subtask_state]);
        }
        if b.state.uses_filesystem_transfers() {
            b.runtime(
                ctx,
                Runtime::FsTransferEvent,
                vec![event, handle, subtask_state],
            );
        }
        let subtask_event = b.i32_const(ctx, EVENT_SUBTASK);
        let is_subtask = b.num(ctx, NumOpcode::I32Eq, vec![event, subtask_event]);
        let returned_state = b.i32_const(ctx, SUBTASK_RETURNED);
        let returned = b.num(ctx, NumOpcode::I32Eq, vec![subtask_state, returned_state]);
        let both = b.num(ctx, NumOpcode::I32And, vec![is_subtask, returned]);
        let settle = b.new_block(ctx, vec![]);
        let next = b.new_block(ctx, vec![]);
        b.cond_br(ctx, both, (settle, vec![]), (next, vec![]));
        b.block = settle;
        let known = b
            .runtime(ctx, Runtime::SubtaskDone, vec![handle])
            .expect("known subtask");
        let drop = b.new_block(ctx, vec![]);
        b.cond_br(ctx, known, (drop, vec![]), (next, vec![]));
        b.block = drop;
        b.host(ctx, crate::Host::SubtaskDrop, vec![handle]);
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

/// Shared lifecycle helpers. Cancellation finishes native operations before any task memory
/// is freed; graceful completion uses the release half after task.return lifts the result.
pub(crate) fn task_cleanup(ctx: &mut Context, state: &mut ModuleState) {
    if !state.needs_task_cleanup && !state.uses_process {
        return;
    }
    let native = state.uses_filesystem
        || state.host.contains(&crate::Host::WaitableSetNew)
        || state.uses_output(Runtime::Stdout)
        || state.uses_output(Runtime::Stderr);
    let void = FunctionType::get(ctx, vec![], vec![]);
    let release = FuncOp::new(ctx, identifier("release_task"), void);
    {
        let mut b = FunctionLowering::glue(state, ctx, &release);
        if native {
            let set = b.runtime(ctx, Runtime::TaskSet, vec![]).expect("set");
            let drop_set = b.new_block(ctx, vec![]);
            let free = b.new_block(ctx, vec![]);
            b.cond_br(ctx, set, (drop_set, vec![]), (free, vec![]));
            b.block = drop_set;
            b.host(ctx, crate::Host::WaitableSetDrop, vec![set]);
            b.br(ctx, free, vec![]);
            b.block = free;
        }
        b.runtime(ctx, Runtime::TaskFree, vec![]);
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, release.get_operation(), 0);
    }
    let abort = FuncOp::new(ctx, identifier("abort_task"), void);
    {
        let mut b = FunctionLowering::glue(state, ctx, &abort);
        if b.state.uses_filesystem_transfers() {
            b.runtime(ctx, Runtime::FsTransfersCancel, vec![]);
        }
        if b.state.uses_filesystem {
            b.runtime(ctx, Runtime::FsCancel, vec![]);
        }
        if b.state.runtime.contains(&Runtime::SubtaskRegister) {
            b.runtime(ctx, Runtime::SubtasksCancel, vec![]);
        }
        for (write, cancel) in [
            (Runtime::Stdout, Runtime::StdoutCancel),
            (Runtime::Stderr, Runtime::StderrCancel),
        ] {
            if b.state.uses_output(write) {
                b.runtime(ctx, cancel, vec![]);
            }
        }
        let call = CallOp::new(ctx, identifier("release_task"), vec![], vec![]).get_operation();
        b.insert(ctx, call);
        let ret = ReturnOp::new(ctx, vec![]).get_operation();
        b.insert(ctx, ret);
        b.state.wasm.append_operation(ctx, abort.get_operation(), 0);
    }
    if state.uses_process {
        let i32 = i32_type(ctx);
        let ty = FunctionType::get(ctx, vec![i32], vec![]);
        let exit = FuncOp::new(ctx, identifier("process_exit"), ty);
        let code = exit.get_entry_block(ctx).deref(ctx).get_argument(0);
        let mut b = FunctionLowering::glue(state, ctx, &exit);
        b.abort_task(ctx);
        b.host(ctx, crate::Host::ProcessExit, vec![code]);
        let unreachable = UnreachableOp::new(ctx).get_operation();
        b.insert(ctx, unreachable);
        b.state.wasm.append_operation(ctx, exit.get_operation(), 0);
    }
}

/// `cabi_realloc`, through which the host lowers strings and lists into guest memory.
pub(crate) fn realloc(ctx: &mut Context, state: &mut ModuleState) {
    let i32 = i32_type(ctx);
    if state.uses_filesystem
        || [Runtime::EnvGet, Runtime::EnvKeys, Runtime::Arguments]
            .iter()
            .any(|capability| state.runtime.contains(capability))
    {
        let ty = FunctionType::get(ctx, vec![i32; 4], vec![i32]);
        let function = FuncOp::new(ctx, identifier("cabi_import_realloc"), ty);
        set_export(ctx, &function, "cabi_import_realloc");
        let args: Vec<Value> = function
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        let mut b = FunctionLowering::glue(state, ctx, &function);
        let result = b
            .runtime(ctx, Runtime::ImportRealloc, args)
            .expect("allocation");
        let ret = ReturnOp::new(ctx, vec![result]).get_operation();
        b.insert(ctx, ret);
        b.state
            .wasm
            .append_operation(ctx, function.get_operation(), 0);
    }

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

fn expect_boolean(ctx: &Context, mir_ty: TypeHandle) -> Result<()> {
    ensure!(
        mir_ty
            .deref(ctx)
            .downcast_ref::<IntegerType>()
            .is_some_and(|ty| ty.width() == 1),
        "WIT bool needs a TypeScript boolean"
    );
    Ok(())
}

impl FunctionLowering<'_> {
    fn abort_if(&mut self, ctx: &mut Context, failed: Value) {
        let trap = self.new_block(ctx, vec![]);
        let done = self.new_block(ctx, vec![]);
        self.cond_br(ctx, failed, (trap, vec![]), (done, vec![]));
        self.block = trap;
        self.abort_task(ctx);
        let unreachable = UnreachableOp::new(ctx).get_operation();
        self.insert(ctx, unreachable);
        self.block = done;
    }

    /// End the synchronous job before its region can be released. Native work cannot
    /// outlive a synchronous export, whose ABI has no callback to resume it.
    pub(crate) fn finish_sync(&mut self, ctx: &mut Context) {
        if self.state.async_functions.is_empty()
            && !self.state.runtime.contains(&Runtime::SubtaskRegister)
        {
            return;
        }
        if !self.state.async_functions.is_empty() {
            let drive =
                CallOp::new(ctx, crate::async_split::drive_name(), vec![], vec![]).get_operation();
            self.insert(ctx, drive);
        }
        let rejected = self
            .runtime(ctx, Runtime::TaskFailed, vec![])
            .expect("rejection status");
        let pending = self
            .runtime(ctx, Runtime::TaskPending, vec![])
            .expect("pending native work");
        let failed = self.num(ctx, NumOpcode::I32Or, vec![rejected, pending]);
        self.abort_if(ctx, failed);
    }

    pub(crate) fn abort_task(&mut self, ctx: &mut Context) {
        self.state.needs_task_cleanup = true;
        let call = CallOp::new(ctx, identifier("abort_task"), vec![], vec![]).get_operation();
        self.insert(ctx, call);
    }

    pub(crate) fn exit_process(&mut self, ctx: &mut Context) {
        let status = self
            .runtime(ctx, Runtime::ExitStatus, vec![])
            .expect("exit status");
        let zero = self.i32_const(ctx, 0);
        let unset = self.num(ctx, NumOpcode::I32LtS, vec![status, zero]);
        let code = self.select(ctx, unset, zero, status);
        let exit = CallOp::new(ctx, identifier("process_exit"), vec![code], vec![]).get_operation();
        self.insert(ctx, exit);
    }

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
                expect_boolean(ctx, crate::optional_inner(ctx, mir_ty).unwrap_or(mir_ty))?;
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

    fn lift_arguments(
        &mut self,
        ctx: &mut Context,
        types: &[AbiType],
        mir_types: &[TypeHandle],
        mut flat: Vec<Value>,
        indirect: bool,
    ) -> Result<Vec<Value>> {
        let mut values = Vec::with_capacity(types.len());
        let mut offset = 0;
        for (ty, mir_ty) in types.iter().zip(mir_types) {
            values.push(if indirect {
                offset = align(offset, ty.alignment());
                let value = self.lift_load(ctx, ty, *mir_ty, flat[0], offset)?;
                offset += ty.size();
                value
            } else {
                self.lift_flat(ctx, ty, *mir_ty, &mut flat)?
            });
        }
        Ok(values)
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
            AbiType::Result { .. } => bail!(
                "WIT result parameters require typed result values, which are not migrated yet"
            ),
            AbiType::String => {
                let (ptr, len) = (next()?, next()?);
                self.lift_string(ctx, mir_ty, ptr, len)
            }
            AbiType::List(element) => {
                let (ptr, len) = (next()?, next()?);
                self.lift_list(ctx, element, mir_ty, ptr, len)
            }
            AbiType::TextOrBytes(order) => {
                let (tag, ptr, len) = (next()?, next()?, next()?);
                self.lift_text_bytes(ctx, *order, mir_ty, tag, ptr, len)
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
            AbiType::Result { .. } => bail!(
                "WIT result parameters require typed result values, which are not migrated yet"
            ),
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
            AbiType::TextOrBytes(order) => {
                let tag = self.load(ctx, MemKind::I32U8, address, offset);
                let ptr = self.load(ctx, MemKind::I32, address, offset + 4);
                let len = self.load(ctx, MemKind::I32, address, offset + 8);
                self.lift_text_bytes(ctx, *order, mir_ty, tag, ptr, len)
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

    fn lift_text_bytes(
        &mut self,
        ctx: &mut Context,
        order: TextBytesOrder,
        mir_ty: TypeHandle,
        tag: Value,
        ptr: Value,
        len: Value,
    ) -> Result<Value> {
        ensure!(
            mir_ty.deref(ctx).is::<TextOrBytesType>(),
            "WIT string/byte-list variant needs TypeScript string | Uint8Array"
        );
        let one = self.i32_const(ctx, 1);
        let invalid = self.num(ctx, NumOpcode::I32GtU, vec![tag, one]);
        self.abort_if(ctx, invalid);
        let text_tag = self.i32_const(ctx, order as i32);
        let is_text = self.num(ctx, NumOpcode::I32Eq, vec![tag, text_tag]);
        let text = self.new_block(ctx, vec![]);
        let bytes = self.new_block(ctx, vec![]);
        let i64 = flat_type(ctx, Flat::I64);
        let done = self.new_block(ctx, vec![i64]);
        self.cond_br(ctx, is_text, (text, vec![]), (bytes, vec![]));
        self.block = text;
        let value = self
            .runtime(ctx, Runtime::StringAdopt, vec![ptr, len])
            .expect("text");
        let value = self.num(ctx, NumOpcode::I64ExtendI32U, vec![value]);
        self.br(ctx, done, vec![value]);
        self.block = bytes;
        let value = self
            .runtime(ctx, Runtime::BytesAdopt, vec![ptr, len])
            .expect("bytes");
        let value = self.num(ctx, NumOpcode::I64ExtendI32U, vec![value]);
        let tag = self.i64_const(ctx, perry_runtime::text_bytes::BYTES_TAG);
        let value = self.num(ctx, NumOpcode::I64Or, vec![value, tag]);
        self.br(ctx, done, vec![value]);
        self.block = done;
        Ok(done.deref(ctx).get_argument(0))
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
        if mir_ty.deref(ctx).is::<perry_mir::types::BytesType>() {
            ensure!(
                matches!(element, AbiType::U8),
                "Uint8Array needs a WIT list<u8>"
            );
            return Ok(self
                .runtime(ctx, Runtime::BytesAdopt, vec![ptr, len])
                .expect("bytes"));
        }
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
            AbiType::Bool => {
                expect_boolean(ctx, mir_ty)?;
                return Ok(value);
            }
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
            AbiType::Result { .. } => {
                bail!("nested WIT results require typed result values, which are not migrated yet")
            }
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
            AbiType::TextOrBytes(order) => {
                let [tag, ptr, len] = self.lower_text_bytes(ctx, *order, mir_ty, value)?;
                self.store(ctx, MemKind::I32U8, address, offset, tag);
                self.store(ctx, MemKind::I32, address, offset + 4, ptr);
                self.store(ctx, MemKind::I32, address, offset + 8, len);
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

    fn lower_text_bytes(
        &mut self,
        ctx: &mut Context,
        order: TextBytesOrder,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<[Value; 3]> {
        let text_tag = self.i32_const(ctx, order as i32);
        let bytes_tag = self.i32_const(ctx, 1 - order as i32);
        if mir_ty.deref(ctx).is::<StringType>() {
            let ptr = self.load(ctx, MemKind::I32, value, strings::DATA);
            let len = self.load(ctx, MemKind::I32, value, strings::LEN);
            return Ok([text_tag, ptr, len]);
        }
        let bytes_ty = BytesType::get(ctx).into();
        if mir_ty == bytes_ty {
            let (ptr, len) = self.lower_list(ctx, &AbiType::U8, bytes_ty, value)?;
            return Ok([bytes_tag, ptr, len]);
        }
        ensure!(
            mir_ty.deref(ctx).is::<TextOrBytesType>(),
            "WIT string/byte-list variant needs TypeScript string | Uint8Array"
        );
        let is_text = self.text_bytes_is_text(ctx, value);
        let tag = self.select(ctx, is_text, text_tag, bytes_tag);
        let reference = self.num(ctx, NumOpcode::I32WrapI64, vec![value]);
        let text = self.new_block(ctx, vec![]);
        let bytes = self.new_block(ctx, vec![]);
        let i32 = i32_type(ctx);
        let done = self.new_block(ctx, vec![i32; 2]);
        self.cond_br(ctx, is_text, (text, vec![]), (bytes, vec![]));
        self.block = text;
        let ptr = self.load(ctx, MemKind::I32, reference, strings::DATA);
        let len = self.load(ctx, MemKind::I32, reference, strings::LEN);
        self.br(ctx, done, vec![ptr, len]);
        self.block = bytes;
        let (ptr, len) = self.lower_list(ctx, &AbiType::U8, bytes_ty, reference)?;
        self.br(ctx, done, vec![ptr, len]);
        self.block = done;
        Ok([
            tag,
            done.deref(ctx).get_argument(0),
            done.deref(ctx).get_argument(1),
        ])
    }

    /// Copy a MIR array into a canonical list buffer; returns (buffer, length).
    fn lower_list(
        &mut self,
        ctx: &mut Context,
        element: &AbiType,
        mir_ty: TypeHandle,
        value: Value,
    ) -> Result<(Value, Value)> {
        if mir_ty.deref(ctx).is::<perry_mir::types::BytesType>() {
            ensure!(
                matches!(element, AbiType::U8),
                "Uint8Array needs a WIT list<u8>"
            );
            let data = self
                .runtime(ctx, Runtime::BytesData, vec![value])
                .expect("byte storage");
            let len = self
                .runtime(ctx, Runtime::BytesLength, vec![value])
                .expect("byte length");
            let len = self.num(ctx, NumOpcode::I32TruncSatF64U, vec![len]);
            return Ok((data, len));
        }
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
            AbiType::Result { .. } => {
                bail!("nested WIT results require typed result values, which are not migrated yet")
            }
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
            AbiType::TextOrBytes(order) => {
                out.extend(self.lower_text_bytes(ctx, *order, mir_ty, value)?);
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
    fn counted_loop(
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
