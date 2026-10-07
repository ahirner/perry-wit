//! Stored tasks run as P3 threads in the resolved component's existing memory.

mod combinators;
mod scheduler;
use super::{PromisePlan, TaskTarget};
use crate::waffle_backend::{
    registry::{ModuleRegistry, PromiseImports},
    resolve::ResolvedContract,
    runtime::builder::{self, Builder},
};
use anyhow::Result;
pub(crate) use scheduler::RUNNABLE as RUNNABLE_SOURCE;
use std::collections::BTreeMap;
use waffle::{Export, ExportKind, Func, Module, Operator, TableData, Type};

pub(crate) struct NativeRuntime {
    pub enter: Func,
    pub combine: Option<Func>,
    pub finish: Func,
    pub validate: Func,
    pub cancel_all: Func,
    pub(crate) settle: Func,
    new_thread: Func,
    resume_now: Func,
    suspend_then_promote: Func,
    yield_then_promote: Func,
    yield_now: Func,
    index: Func,
    suspend: Func,
    resume: Func,
    context_get: Func,
    context_set: Func,
    schedule: Func,
    observe: Func,
    pause: Func,
    pub complete: Func,
}

pub(crate) fn declare(module: &mut Module<'static>, plan: &PromisePlan) -> Result<PromiseImports> {
    use Type::{F64, I32};
    let native = NativeRuntime {
        new_thread: builder::native(module, "[thread-new-indirect-v0]", &[I32, I32], &[I32]),
        resume_now: builder::native(module, "[thread-yield-then-resume]", &[I32], &[I32]),
        suspend_then_promote: builder::native(
            module,
            "[thread-suspend-then-promote]",
            &[I32],
            &[I32],
        ),
        yield_then_promote: builder::native(module, "[thread-yield-then-promote]", &[I32], &[I32]),
        yield_now: builder::native(module, "[thread-yield]", &[], &[I32]),
        index: builder::native(module, "[thread-index]", &[], &[I32]),
        suspend: builder::native(module, "[thread-suspend]", &[], &[I32]),
        resume: builder::native(module, "[thread-resume-later]", &[I32], &[]),
        context_get: builder::native(module, "[context-get-0]", &[], &[I32]),
        context_set: builder::native(module, "[context-set-0]", &[I32], &[]),
        schedule: builder::declare(module, "tasks.wake", &[I32], &[]),
        observe: builder::declare(module, "tasks.observe-promise", &[I32, I32], &[]),
        pause: builder::declare(module, "tasks.pause", &[], &[]),
        complete: builder::declare(module, "tasks.complete", &[], &[]),
        combine: plan
            .combinators
            .then(|| builder::declare(module, "tasks.combine", &[I32; 6], &[I32])),
        enter: builder::declare(module, "tasks.enter", &[], &[]),
        finish: builder::declare(module, "tasks.finish", &[], &[]),
        validate: builder::declare(module, "tasks.validate", &[], &[]),
        cancel_all: builder::declare(module, "tasks.cancel-all", &[], &[]),
        settle: builder::declare(module, "tasks.settle", &[I32, I32, F64], &[]),
    };
    let new = builder::declare(module, "tasks.new", &[I32], &[I32]);
    let await_result = builder::declare(module, "tasks.await", &[I32], &[I32, F64]);
    let await_native = builder::declare(module, "tasks.join-native", &[I32], &[I32, F64]);
    let yield_thread = builder::declare(module, "tasks.yield", &[], &[]);
    let mut starts = BTreeMap::new();
    for (target, task) in &plan.tasks {
        let mut params = vec![I32];
        params.extend(task.arguments.core_types()?);
        starts.insert(
            target.clone(),
            builder::declare(module, &task.symbol, &params, &[I32]),
        );
    }
    Ok(PromiseImports {
        new,
        await_result,
        await_native,
        yield_thread,
        starts,
        native,
    })
}

fn allocate(b: &mut Builder, registry: &ModuleRegistry, size: u32) -> waffle::Value {
    let zero = b.integer(0);
    let alignment = b.integer(8);
    let size = b.integer(size);
    b.call(
        registry.allocator.unwrap().realloc,
        &[zero, zero, alignment, size],
        &[Type::I32],
    )[0]
}

pub(crate) fn emit(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    contract: &ResolvedContract,
    strings: &crate::waffle_backend::strings::StringPool,
) -> Result<Vec<crate::waffle_backend::constant_arguments::CapturedArguments>> {
    use Type::{F64, I32};
    let runtime = registry.promises.as_ref().unwrap();
    let native = &runtime.native;
    let memory = registry.memory;
    let mut b = Builder::new(module, runtime.new, memory);
    let kind = b.param(0);
    let record = allocate(&mut b, registry, 32);
    let zero = b.integer(0);
    let size = b.integer(32);
    b.effect(Operator::MemoryFill { mem: memory }, &[record, zero, size]);
    let four = b.integer(4);
    let header = b.op(Operator::I32Sub, &[record, four], I32);
    let header = b.load(header, 0, I32);
    b.store(header, 16, kind, I32);
    let pending = b.integer(2);
    b.store(record, 4, pending, I32);
    let head = b.load(four, 0, I32);
    b.store(record, 16, head, I32);
    b.store(four, 0, record, I32);
    if let Some(operations) = registry.operations {
        let cancelled = b.call(operations.cancelled, &[], &[I32])[0];
        let cancel = b.body.add_block();
        let done = b.body.add_block();
        b.branch(cancelled, cancel, done);
        b.block = cancel;
        let thrown = b.integer(1);
        let reason = b.number(20.0);
        b.call(native.settle, &[record, thrown, reason], &[]);
        b.jump(done, &[]);
        b.block = done;
    }
    b.ret(&[record]);
    b.finish(module, runtime.new)?;

    let mut b = Builder::new(module, native.enter, memory);
    let address = b.integer(32);
    let active = b.load(address, 0, I32);
    let inactive = b.op(Operator::I32Eqz, &[active], I32);
    b.require(inactive);
    let one = b.integer(1);
    b.store(address, 0, one, I32);
    let active = b.integer(scheduler::RUNNABLE);
    b.store(active, 0, one, I32);
    let workers = b.integer(crate::waffle_backend::runtime::callbacks::LIVE_WORKERS);
    let zero = b.integer(0);
    b.store(workers, 0, zero, I32);
    b.ret(&[]);
    b.finish(module, native.enter)?;

    scheduler::emit(module, registry)?;
    emit_finish(module, registry)?;
    emit_await(module, registry)?;
    emit_native_await(module, registry)?;
    emit_settle(module, registry)?;
    emit_cancel_all(module, registry)?;

    let mut workers = Vec::new();
    let mut captures = Vec::new();
    for (target, task) in &contract.promises.as_ref().unwrap().tasks {
        let start = runtime.starts[target];
        let parameters = task.arguments.core_types()?;
        let worker = builder::declare(module, &format!("{}.worker", task.symbol), &[I32], &[]);
        let mut b = Builder::new(module, start, memory);
        let context = allocate(&mut b, registry, 8 * (parameters.len() as u32 + 1));
        let one = b.integer(1);
        let start_frame = b.call(registry.allocator.unwrap().frame_new, &[one], &[I32])[0];
        b.store(start_frame, 12, context, I32);
        b.store(context, 0, b.param(0), I32);
        let guest = matches!(target, TaskTarget::Guest(_));
        if guest {
            scheduler::update_runnable(&mut b, true);
            let parent = b.call(native.index, &[], &[I32])[0];
            let eager = b.op(Operator::I32Add, &[parent, one], I32);
            b.store(context, 4, eager, I32);
        }
        for (index, ty) in parameters.iter().enumerate() {
            b.store(context, 8 * (index as u32 + 1), b.param(index + 1), *ty);
        }
        let index = b.integer(workers.len() as u32);
        crate::waffle_backend::runtime::callbacks::worker_count(
            &mut b,
            true,
            if guest {
                crate::waffle_backend::runtime::callbacks::Worker::Source
            } else {
                crate::waffle_backend::runtime::callbacks::Worker::Native
            },
        );
        let thread = b.call(native.new_thread, &[index, context], &[I32])[0];
        b.call(native.resume_now, &[thread], &[I32]);
        let zero = b.integer(0);
        if guest {
            b.store(context, 4, zero, I32);
        }
        b.call(registry.allocator.unwrap().frame_drop, &[start_frame], &[]);
        b.ret(&[zero]);
        b.finish(module, start)?;

        let filesystem = match task.arguments {
            super::TaskArguments::Filesystem(operation) => {
                Some(filesystem_adapter(module, registry, operation)?)
            }
            _ => None,
        };
        let mut b = Builder::new(module, worker, memory);
        let context = b.param(0);
        let record = b.load(context, 0, I32);
        if guest {
            b.call(native.context_set, &[context], &[]);
        }
        let mut args = parameters
            .iter()
            .enumerate()
            .map(|(index, ty)| b.load(context, 8 * (index as u32 + 1), *ty))
            .collect::<Vec<_>>();
        captures.push(
            crate::waffle_backend::constant_arguments::CapturedArguments {
                start,
                worker,
                loads: args.clone(),
            },
        );
        let mut roots = vec![record, context];
        roots.extend(
            parameters
                .iter()
                .zip(&args)
                .filter_map(|(ty, value)| (*ty == I32).then_some(*value)),
        );
        let count = b.integer(roots.len() as u32);
        let frame = b.call(registry.allocator.unwrap().frame_new, &[count], &[I32])[0];
        for (index, value) in roots.iter().enumerate() {
            b.store(frame, 12 + 4 * index as u32, *value, I32);
        }
        let (callee, completion) = match target {
            TaskTarget::WebStream(method) => {
                args.push(record);
                (registry.web_streams.unwrap().method(*method), true)
            }
            TaskTarget::Guest(id) => (registry.functions[id].func_index, true),
            TaskTarget::FetchUpload => (registry.fetch_helpers.unwrap().upload, true),
            TaskTarget::HttpBody(method) => (registry.body_helpers.unwrap().method(*method), true),
            TaskTarget::Intrinsic(name) => (
                filesystem.unwrap_or_else(|| registry.intrinsics[name]),
                contract.intrinsics[name].has_completion(),
            ),
        };
        let results = module.signatures[module.funcs[callee].sig()]
            .returns
            .clone();
        let result = b.call(callee, &args, &results);
        let (tag, payload) = if completion {
            (result[0], result[1])
        } else {
            let tag = b.integer(0);
            let payload = match result.first() {
                None => b.op(Operator::F64Const { value: 0 }, &[], F64),
                Some(value) if results[0] == I32 => b.op(Operator::F64ConvertI32U, &[*value], F64),
                Some(value) => *value,
            };
            (tag, payload)
        };
        if matches!(
            target,
            TaskTarget::WebStream(
                super::super::streams::web::Method::Read
                    | super::super::streams::web::Method::ReadInto
            )
        ) {
            let state = b.load(record, 4, I32);
            let two = b.integer(2);
            let pending = b.op(Operator::I32Eq, &[state, two], I32);
            let settle = b.body.add_block();
            let done = b.body.add_block();
            b.branch(pending, settle, done);
            b.block = settle;
            b.call(native.settle, &[record, tag, payload], &[]);
            b.jump(done, &[]);
            b.block = done;
        } else {
            b.call(native.settle, &[record, tag, payload], &[]);
        }
        b.call(registry.allocator.unwrap().frame_drop, &[frame], &[]);
        if guest {
            b.call(native.complete, &[], &[]);
        }
        crate::waffle_backend::runtime::callbacks::worker_count(
            &mut b,
            false,
            if guest {
                crate::waffle_backend::runtime::callbacks::Worker::Source
            } else {
                crate::waffle_backend::runtime::callbacks::Worker::Native
            },
        );
        b.ret(&[]);
        b.finish(module, worker)?;
        workers.push(worker);
    }
    if native.combine.is_some() {
        workers.push(combinators::emit(
            module,
            registry,
            workers.len() as u32,
            strings,
        )?);
    }
    let table = module.tables.push(TableData {
        ty: Type::FuncRef,
        initial: workers.len() as u64,
        max: Some(workers.len() as u64),
        func_elements: Some(workers),
    });
    module.exports.push(Export {
        name: "__indirect_function_table".into(),
        kind: ExportKind::Table(table),
    });
    Ok(captures)
}

fn filesystem_adapter(
    module: &mut Module<'static>,
    registry: &ModuleRegistry,
    operation: crate::waffle_backend::capabilities::FilesystemOperation,
) -> Result<Func> {
    use crate::waffle_backend::capabilities::FilesystemOperation as F;
    use Type::{F64, I32};
    let parameters = super::TaskArguments::Filesystem(operation).core_types()?;
    let function = builder::declare(module, "tasks.filesystem", &parameters, &[I32, F64]);
    let mut b = Builder::new(module, function, registry.memory);
    let helpers = registry.filesystem_helpers.unwrap();
    let arguments = (0..parameters.len() - 1)
        .map(|index| b.param(index))
        .collect::<Vec<_>>();
    if let Some(operations) = registry.operations {
        let signal = b.param(parameters.len() - 1);
        b.call(operations.bind_signal, &[signal], &[]);
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        let failed = b.body.add_block();
        let start = b.body.add_block();
        b.branch(aborted, failed, start);
        b.block = failed;
        let thrown = b.integer(1);
        let reason = b.number(20.0);
        b.ret(&[thrown, reason]);
        b.block = start;
    }
    let outcome = match operation {
        F::WriteFile => b.call(helpers.write, &arguments, &[I32, F64]),
        F::ReadBytes | F::ReadText | F::ReadValue => b.call(helpers.read, &arguments, &[I32, F64]),
        F::ReadDirectory => b.call(helpers.read_directory, &arguments, &[I32, F64]),
        operation => {
            let code = b.integer(match operation {
                F::Stat => 0,
                F::MakeDirectory => 1,
                F::Unlink => 2,
                F::RemoveDirectory => 3,
                _ => unreachable!(),
            });
            b.call(
                helpers.metadata,
                &[arguments[0], code, arguments[1]],
                &[I32, F64],
            )
        }
    };
    if let Some(operations) = registry.operations {
        let aborted = b.call(operations.aborted, &[], &[I32])[0];
        let failed = b.body.add_block();
        let complete = b.body.add_block();
        b.branch(aborted, failed, complete);
        b.block = failed;
        let thrown = b.integer(1);
        let reason = b.number(20.0);
        b.ret(&[thrown, reason]);
        b.block = complete;
    }
    if operation == F::ReadValue {
        let success = b.op(Operator::I32Eqz, &[outcome[0]], I32);
        let convert = b.body.add_block();
        let rejected = b.body.add_block();
        b.branch(success, convert, rejected);
        b.block = rejected;
        b.ret(&outcome);
        b.block = convert;
        let pointer = b.op(Operator::I32TruncF64U, &[outcome[1]], I32);
        let binary = b.op(Operator::I32Eqz, &[arguments[1]], I32);
        let tagged = b.op(Operator::I32Or, &[pointer, binary], I32);
        let payload = b.op(Operator::F64ConvertI32U, &[tagged], F64);
        b.ret(&[outcome[0], payload]);
    } else {
        b.ret(&outcome);
    }
    b.finish(module, function)?;
    Ok(function)
}

fn emit_finish(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::I32;
    let runtime = registry.promises.as_ref().unwrap();
    let native = &runtime.native;
    let function = native.validate;
    let mut b = Builder::new(module, function, registry.memory);
    {
        let check = b.body.add_block();
        let drain = b.body.add_block();
        let validate = b.body.add_block();
        b.jump(check, &[]);
        b.block = check;
        let address = b.integer(scheduler::RUNNABLE);
        let count = b.load(address, 0, I32);
        let one = b.integer(1);
        let other = b.op(Operator::I32GtU, &[count, one], I32);
        b.branch(other, drain, validate);
        b.block = drain;
        b.call(runtime.yield_thread, &[], &[]);
        b.jump(check, &[]);
        b.block = validate;
    }
    let address = b.integer(4);
    let head = b.load(address, 0, I32);
    let next = b.body.add_block();
    let record = b.body.add_blockparam(next, I32);
    let check = b.body.add_block();
    let done = b.body.add_block();
    b.jump(next, &[head]);
    b.block = next;
    b.branch(record, check, done);
    b.block = check;
    let state = b.load(record, 0, I32);
    let settled = b.integer(2);
    let complete = b.op(Operator::I32Eq, &[state, settled], I32);
    b.require(complete);
    let record = b.load(record, 16, I32);
    b.jump(next, &[record]);
    b.block = done;
    b.ret(&[]);
    b.finish(module, function)?;
    let function = native.finish;
    let mut b = Builder::new(module, function, registry.memory);
    b.call(native.validate, &[], &[]);
    if registry.callbacks.is_none() {
        let check = b.body.add_block();
        let drain = b.body.add_block();
        let finished = b.body.add_block();
        b.jump(check, &[]);
        b.block = check;
        let workers = b.integer(crate::waffle_backend::runtime::callbacks::LIVE_WORKERS);
        let workers = b.load(workers, 0, I32);
        b.branch(workers, drain, finished);
        b.block = drain;
        b.call(runtime.native.yield_now, &[], &[I32]);
        b.jump(check, &[]);
        b.block = finished;
    }
    let zero = b.integer(0);
    let address = b.integer(4);
    b.store(address, 0, zero, I32);
    let active = b.integer(32);
    b.store(active, 0, zero, I32);
    let active = b.integer(scheduler::RUNNABLE);
    b.store(active, 0, zero, I32);
    b.ret(&[]);
    b.finish(module, function)
}

fn emit_await(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::{F64, I32};
    let runtime = registry.promises.as_ref().unwrap();
    let native = &runtime.native;
    let mut b = Builder::new(module, runtime.await_result, registry.memory);
    let record = b.param(0);
    let two = b.integer(2);
    let tag = b.load(record, 4, I32);
    let pending = b.op(Operator::I32Eq, &[tag, two], I32);
    let wait = b.body.add_block();
    let yield_ready = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(pending, wait, yield_ready);
    b.block = wait;
    let thread = b.call(native.index, &[], &[I32])[0];
    b.call(native.observe, &[record, thread], &[]);
    b.call(native.pause, &[], &[]);
    b.jump(ready, &[]);
    b.block = yield_ready;
    b.call(runtime.yield_thread, &[], &[]);
    b.jump(ready, &[]);
    b.block = ready;
    let tag = b.load(record, 4, I32);
    let settled = b.op(Operator::I32LtU, &[tag, two], I32);
    b.require(settled);
    let payload = b.load(record, 8, F64);
    b.ret(&[tag, payload]);
    b.finish(module, runtime.await_result)
}

/// Native transport joins retain their observers independently of source continuations.
fn emit_native_await(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::{F64, I32};
    let runtime = registry.promises.as_ref().unwrap();
    let native = &runtime.native;
    let mut b = Builder::new(module, runtime.await_native, registry.memory);
    let record = b.param(0);
    let tag = b.load(record, 4, I32);
    let two = b.integer(2);
    let pending = b.op(Operator::I32Eq, &[tag, two], I32);
    let wait = b.body.add_block();
    let ready = b.body.add_block();
    b.branch(pending, wait, ready);
    b.block = wait;
    let thread = b.call(native.index, &[], &[I32])[0];
    let waiter = scheduler::waiter(&mut b, registry, thread);
    let previous = b.load(record, 28, I32);
    b.store(waiter, 4, previous, I32);
    b.store(record, 28, waiter, I32);
    b.call(native.suspend, &[], &[I32]);
    b.jump(ready, &[]);
    b.block = ready;
    let tag = b.load(record, 4, I32);
    let settled = b.op(Operator::I32LtU, &[tag, two], I32);
    b.require(settled);
    let payload = b.load(record, 8, F64);
    b.ret(&[tag, payload]);
    b.finish(module, runtime.await_native)
}

fn emit_settle(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::{F64, I32};
    let native = &registry.promises.as_ref().unwrap().native;
    let mut b = Builder::new(module, native.settle, registry.memory);
    let record = b.param(0);
    let tag = b.param(1);
    let payload = b.param(2);
    let two = b.integer(2);
    let old_tag = b.load(record, 4, I32);
    let pending = b.op(Operator::I32Eq, &[old_tag, two], I32);
    if let Some(operations) = registry.operations {
        let settle = b.body.add_block();
        let duplicate = b.body.add_block();
        b.branch(pending, settle, duplicate);
        b.block = duplicate;
        let cancelled = b.call(operations.cancelled, &[], &[I32])[0];
        b.require(cancelled);
        b.ret(&[]);
        b.block = settle;
    } else {
        b.require(pending);
    }
    let valid = b.op(Operator::I32LtU, &[tag, two], I32);
    b.require(valid);
    b.store(record, 8, payload, F64);
    b.store(record, 4, tag, I32);
    b.store(record, 0, two, I32);
    let zero = b.integer(0);
    for (offset, resume) in [(20, native.schedule), (28, native.resume)] {
        let head = b.load(record, offset, I32);
        b.store(record, offset, zero, I32);
        if offset == 20 {
            b.store(record, 24, zero, I32);
        }
        let next = b.body.add_block();
        let waiter = b.body.add_blockparam(next, I32);
        let wake = b.body.add_block();
        let done = b.body.add_block();
        b.jump(next, &[head]);
        b.block = next;
        b.branch(waiter, wake, done);
        b.block = wake;
        let thread = b.load(waiter, 0, I32);
        let following = b.load(waiter, 4, I32);
        b.call(resume, &[thread], &[]);
        let eight = b.integer(8);
        b.call(
            registry.allocator.unwrap().realloc,
            &[waiter, eight, eight, zero],
            &[I32],
        );
        b.jump(next, &[following]);
        b.block = done;
    }
    b.ret(&[]);
    b.finish(module, native.settle)
}

fn emit_cancel_all(module: &mut Module<'static>, registry: &ModuleRegistry) -> Result<()> {
    use Type::I32;
    let native = &registry.promises.as_ref().unwrap().native;
    let mut b = Builder::new(module, native.cancel_all, registry.memory);
    if let Some(operations) = registry.operations {
        let cancelled = b.call(operations.cancelled, &[], &[I32])[0];
        b.require(cancelled);
        let address = b.integer(4);
        let head = b.load(address, 0, I32);
        let next = b.body.add_block();
        let record = b.body.add_blockparam(next, I32);
        let visit = b.body.add_block();
        let done = b.body.add_block();
        b.jump(next, &[head]);
        b.block = next;
        b.branch(record, visit, done);
        b.block = visit;
        let state = b.load(record, 4, I32);
        let two = b.integer(2);
        let pending = b.op(Operator::I32Eq, &[state, two], I32);
        let cancel = b.body.add_block();
        let following = b.body.add_block();
        b.branch(pending, cancel, following);
        b.block = cancel;
        let thrown = b.integer(1);
        let reason = b.number(20.0);
        b.call(native.settle, &[record, thrown, reason], &[]);
        b.jump(following, &[]);
        b.block = following;
        let successor = b.load(record, 16, I32);
        b.jump(next, &[successor]);
        b.block = done;
    }
    b.ret(&[]);
    b.finish(module, native.cancel_all)
}
