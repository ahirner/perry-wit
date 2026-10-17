//! Canonical export wrappers, async callbacks and reallocation.
use super::*;

/// Emit the export function and its post-return function for `export`.
pub(crate) fn export(
    ctx: &mut Context,
    state: &mut ModuleState,
    function: &FuncOp,
    lowered: &FuncOp,
    export: &Export,
) -> Result<()> {
    let scope = Scope::for_export(state, export, &function.get_symbol_name(ctx));
    let is_async = state
        .async_functions
        .contains(&function.get_symbol_name(ctx));
    ensure!(
        is_async == export.is_async(),
        "{} is {}async but the WIT export is {}async",
        export.function,
        if is_async { "" } else { "not " },
        if export.is_async() { "" } else { "not " },
    );
    if let ExportKind::Async {
        task_return,
        command,
    } = &export.kind
    {
        return async_export(ctx, state, function, export, task_return, *command);
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
    let completion = export
        .result
        .as_ref()
        .and_then(|ty| ty.completion(ctx, mir_results.first().copied()));
    ensure!(
        completion.is_some() || mir_results.len() == usize::from(export.result.is_some()),
        "{} returns {} values but the WIT export returns {}",
        export.function,
        mir_results.len(),
        usize::from(export.result.is_some())
    );
    let flat_params = parameters::export_signature(&export.params);
    let flat_result = export
        .result
        .as_ref()
        .map(AbiType::flat)
        .unwrap_or_default();
    let flat_result_len = flat_result.len();
    let core_results: Vec<Flat> = if flat_result_len > 1 {
        vec![Flat::I32]
    } else {
        flat_result
    };

    let params: Vec<_> = flat_params.iter().map(|f| flat_type(ctx, *f)).collect();
    let results: Vec<_> = core_results.iter().map(|f| flat_type(ctx, *f)).collect();
    let post_ty = FunctionType::get(ctx, results.clone(), vec![]);
    let fn_ty = FunctionType::get(ctx, params, results);
    let glue = FuncOp::new(ctx, identifier(&export.core_name), fn_ty);
    set_export(ctx, &glue, &export.core_name);
    let entry = glue.get_entry_block(ctx);
    let mut flat: Vec<Value> = entry.deref(ctx).arguments().collect();
    flat.reverse();

    let mut b = FunctionLowering::glue(state, ctx, &glue);
    if scope == Scope::Task {
        b.task_enter(ctx);
    }
    let initialization = b.initialization_status(ctx);
    let mut joined_types = mir_results
        .iter()
        .map(|&ty| wasm_type(ctx, ty))
        .collect::<Result<Vec<_>>>()?;
    joined_types.push(i32_type(ctx));
    let joined = initialization.map(|status| {
        let active = b.new_block(ctx, vec![]);
        let failed = b.new_block(ctx, vec![]);
        let joined = b.new_block(ctx, joined_types.clone());
        b.cond_br(ctx, status, (failed, vec![]), (active, vec![]));
        b.block = failed;
        let mut values: Vec<_> = joined_types[..joined_types.len() - 1]
            .iter()
            .map(|&ty| b.zero(ctx, ty))
            .collect();
        values.push(status);
        b.br(ctx, joined, values);
        b.block = active;
        joined
    });
    let args = b.lift_parameters(ctx, &export.params, &mir_params, &mut flat)?;
    let throws = b.state.may_throw(&function.get_symbol_name(ctx));
    let mut call_results = mir_results
        .iter()
        .map(|t| wasm_type(ctx, *t))
        .collect::<Result<Vec<_>>>()?;
    if throws {
        call_results.push(i32_type(ctx));
    }
    let call = CallOp::new(ctx, function.get_symbol_name(ctx), args, call_results).get_operation();
    b.insert(ctx, call);
    let mut values: Vec<Value> = call.deref(ctx).results().collect();
    if let Some(joined) = joined {
        if !throws {
            values.push(b.i32_const(ctx, 0));
        }
        b.br(ctx, joined, values);
        b.block = joined;
        values = joined.deref(ctx).arguments().collect();
    }
    let status = if throws || joined.is_some() {
        values.pop()
    } else {
        None
    };
    if let Some(status) = status {
        // An uncaught exception without a WIT error type has no value to return.
        let trap = b.new_block(ctx, vec![]);
        let normal = b.new_block(ctx, vec![]);
        b.cond_br(ctx, status, (trap, vec![]), (normal, vec![]));
        b.block = trap;
        b.return_resources(ctx);
        if let Some(completion) = export
            .result
            .as_ref()
            .filter(|r| r.accepts_exception())
            .and_then(AbiType::error_completion)
        {
            let ty = completion.result;
            let exception = b.runtime(ctx, Runtime::ExceptionCatch, vec![status]);
            let payload = b.completion_error(ctx, completion, exception)?;
            let returned = if flat_result_len == 1 {
                b.completion_flat(ctx, completion, 1, payload)?
            } else {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.alloc(ctx, size);
                b.completion_store(ctx, completion, 1, payload, area)?;
                vec![area]
            };
            let ret = ReturnOp::new(ctx, returned).get_operation();
            b.insert(ctx, ret);
        } else {
            b.task_free(ctx);
            let unreachable = UnreachableOp::new(ctx).get_operation();
            b.insert(ctx, unreachable);
        }
        b.block = normal;
    }
    if b.state.collectible
        && mir_results
            .first()
            .is_some_and(|ty| repr(ctx, *ty).is_ok_and(|r| r == Repr::Ref))
    {
        b.runtime(ctx, Runtime::GcPin, vec![values[0]]);
    }
    b.return_resources(ctx);
    b.begin_resources(
        ctx,
        export.result.as_ref().is_some_and(AbiType::has_resources),
        None,
    )?;
    let returned = if let Some(completion) = completion {
        let ty = completion.result;
        let payload = mir_results
            .first()
            .map(|ty| (*ty, Carriers::Scalar(values[0])));
        if flat_result_len == 1 {
            b.completion_flat(ctx, completion, 0, payload)?
        } else {
            let size = b.i32_const(ctx, ty.size() as i32);
            let area = b.alloc(ctx, size);
            b.completion_store(ctx, completion, 0, payload, area)?;
            vec![area]
        }
    } else {
        match (&export.result, mir_results.first()) {
            (Some(ty), Some(mir_ty)) if flat_result_len == 1 => {
                let mut flat = Vec::new();
                b.lower_flat_values(ctx, ty, *mir_ty, Carriers::Scalar(values[0]), &mut flat)?;
                flat
            }
            (Some(ty), Some(mir_ty)) => {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.alloc(ctx, size);
                b.lower_store(ctx, ty, *mir_ty, Carriers::Scalar(values[0]), area, 0)?;
                vec![area]
            }
            _ => vec![],
        }
    };
    b.commit_resources(ctx);
    b.finish_resources(ctx);
    let ret = ReturnOp::new(ctx, returned).get_operation();
    b.insert(ctx, ret);
    let state = b.state;
    crate::module::append_export(ctx, state, &glue, lowered, &export.core_name);

    if scope == Scope::Direct {
        return Ok(());
    }

    // `cabi_post_<export>` receives the core results and ends the task.
    let post_name = format!("cabi_post_{}", export.core_name);
    let post = FuncOp::new(ctx, identifier(&post_name), post_ty);
    set_export(ctx, &post, &post_name);
    let mut b = FunctionLowering::glue(state, ctx, &post);
    b.task_free(ctx);
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
    (module, wit_name): &(String, String),
    command: bool,
) -> Result<()> {
    let ty = TypedHandle::<FunctionType>::from_handle(function.get_type(ctx), ctx)
        .map_err(|_| anyhow!("function type"))?;
    let (mir_params, mir_results) = {
        let ty = ty.deref(ctx);
        (ty.arg_types(), ty.res_types())
    };
    let completion = export
        .result
        .as_ref()
        .and_then(|ty| ty.completion(ctx, mir_results.first().copied()));
    ensure!(
        mir_params.len() == export.params.len(),
        "{} takes {} parameters but the WIT export has {}",
        export.function,
        mir_params.len(),
        export.params.len()
    );
    let flat_params = parameters::export_signature(&export.params);
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
    let task_return = EffectHost::TaskReturn(
        module.clone(),
        wit_name.clone(),
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
        if uses_io {
            b.runtime_void(ctx, Runtime::IoPump, vec![]);
        }
        let mut promise = b.runtime(ctx, Runtime::TaskResult, vec![]);
        let mut settled = b.load(
            ctx,
            MemKind::I32,
            promise,
            perry_abi::layout::promise::STATE,
        );
        let wait = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.cond_br(ctx, settled, (done, vec![]), (wait, vec![]));

        // Pending: leave the task and wait on its subtasks (none would wait forever).
        b.block = wait;
        let set = b.runtime(ctx, Runtime::TaskSuspend, vec![]);
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
        // Propagate combinator cancellation ownership before checking helper-owned work.
        let mut detached = b.runtime(ctx, Runtime::TaskCancel, vec![]);
        for (used, function) in [
            (b.state.uses_fetch, Runtime::WebFetchPending),
            (b.state.uses_streams, Runtime::WebStreamsPending),
        ] {
            if used {
                let pending = b.runtime(ctx, function, vec![]);
                detached = b.num(ctx, NumOpcode::I32Add, vec![detached, pending]);
            }
        }
        b.cleanup_foreign(ctx)?;
        if b.state.uses_fs {
            let filesystem = b.runtime(ctx, Runtime::FsClose, vec![]);
            detached = b.num(ctx, NumOpcode::I32Add, vec![detached, filesystem]);
        }
        if command {
            let message = b
                .state
                .data
                .string("Unresolved detached work at task boundary; pending operations cancelled");
            let message = b.i32_const(ctx, message as i32);
            b.runtime_void(ctx, Runtime::TaskDetached, vec![detached, message]);
        } else {
            let clean = b.new_block(ctx, vec![]);
            let failed = b.new_block(ctx, vec![]);
            let completion = b.new_block(ctx, vec![i32, i32]);
            b.cond_br(ctx, detached, (failed, vec![]), (clean, vec![]));
            b.block = failed;
            let rejected = b.i32_const(ctx, perry_abi::layout::promise::REJECTED as i32);
            let preserve = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected]);
            let rejected_completion = b.new_block(ctx, vec![]);
            b.cond_br(
                ctx,
                preserve,
                (completion, vec![promise, settled]),
                (rejected_completion, vec![]),
            );
            b.block = rejected_completion;
            let message = b
                .state
                .data
                .string("Unresolved detached work at task boundary; pending operations cancelled");
            let message = b.i64_const(ctx, message as u64);
            let kind = b.i32_const(ctx, 3);
            let tag = b.i32_const(ctx, 3);
            let exception = b.runtime(ctx, Runtime::ExceptionNew, vec![kind, tag, message]);
            let status = b.runtime(ctx, Runtime::ExceptionRaise, vec![exception]);
            let failure = b.runtime(ctx, Runtime::PromiseNew, vec![]);
            b.runtime_void(ctx, Runtime::PromiseReject, vec![failure, status]);
            b.runtime_void(ctx, Runtime::TaskSetResult, vec![failure]);
            b.br(ctx, completion, vec![failure, rejected]);
            b.block = clean;
            b.br(ctx, completion, vec![promise, settled]);
            b.block = completion;
            promise = completion.deref(ctx).get_argument(0);
            settled = completion.deref(ctx).get_argument(1);
        }
        let outcome = if command {
            Some(b.runtime(ctx, Runtime::CommandOutcome, vec![promise]))
        } else {
            let rejected_state = b.i32_const(ctx, perry_abi::layout::promise::REJECTED as i32);
            let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
            let trap = b.new_block(ctx, vec![]);
            let fulfilled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, rejected, (trap, vec![]), (fulfilled, vec![]));
            b.block = trap;
            b.return_resources(ctx);
            if uses_io
                && export
                    .result
                    .as_ref()
                    .is_some_and(AbiType::accepts_exception)
            {
                let flushed = b.runtime(ctx, Runtime::IoClose, vec![]);
                let closed = b.new_block(ctx, vec![]);
                b.cond_br(ctx, flushed, (closed, vec![]), (wait, vec![]));
                b.block = closed;
            }
            if export
                .result
                .as_ref()
                .is_some_and(AbiType::accepts_exception)
            {
                let mut values = Vec::new();
                if let Some(completion) = export.result.as_ref().and_then(AbiType::error_completion)
                {
                    let ty = completion.result;
                    let status = b.runtime(ctx, Runtime::PromiseRethrow, vec![promise]);
                    let exception = b.runtime(ctx, Runtime::ExceptionCatch, vec![status]);
                    let payload = b.completion_error(ctx, completion, exception)?;
                    if indirect {
                        let size = b.i32_const(ctx, ty.size() as i32);
                        let area = b.alloc(ctx, size);
                        b.completion_store(ctx, completion, 1, payload, area)?;
                        values.push(area);
                    } else {
                        values = b.completion_flat(ctx, completion, 1, payload)?;
                    }
                } else if let Some(AbiType::OkHandle { flat, error_case }) = &export.result {
                    // `err(error_case(some(description)))`, then end the task.
                    let text = b.runtime(ctx, Runtime::RejectionText, vec![promise]);
                    let data = b.load(ctx, MemKind::I32, text, strings::DATA);
                    let len = b.load(ctx, MemKind::I32, text, strings::LEN);
                    let one = b.i32_const(ctx, 1);
                    let case = b.i32_const(ctx, *error_case as i32);
                    b.flat_i32s(ctx, flat, &[one, case, one, data, len], &mut values)?;
                }
                if b.state.uses_http_server {
                    b.runtime_void(ctx, Runtime::WebServeClose, vec![]);
                }
                if b.state.uses_fetch {
                    b.runtime_void(ctx, Runtime::WebFetchClose, vec![]);
                }
                if b.state.uses_streams {
                    b.runtime_void(ctx, Runtime::WebStreamsClose, vec![]);
                }
                b.host_void(ctx, task_return.clone(), values);
                b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
                b.task_free(ctx);
                let exit = b.i32_const(ctx, EXIT);
                let ret = ReturnOp::new(ctx, vec![exit]).get_operation();
                b.insert(ctx, ret);
            } else {
                if uses_io || b.state.reports_rejection {
                    b.runtime_void(ctx, Runtime::ReportRejection, vec![promise]);
                    let flushed = b.runtime(ctx, Runtime::IoClose, vec![]);
                    let closed = b.new_block(ctx, vec![]);
                    b.cond_br(ctx, flushed, (closed, vec![]), (wait, vec![]));
                    b.block = closed;
                }
                if b.state.uses_http_server {
                    b.runtime_void(ctx, Runtime::WebServeClose, vec![]);
                }
                if b.state.uses_fetch {
                    b.runtime_void(ctx, Runtime::WebFetchClose, vec![]);
                }
                if b.state.uses_streams {
                    b.runtime_void(ctx, Runtime::WebStreamsClose, vec![]);
                }
                b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
                b.return_resources(ctx);
                b.task_free(ctx);
                let unreachable = UnreachableOp::new(ctx).get_operation();
                b.insert(ctx, unreachable);
            }
            b.block = fulfilled;
            None
        };
        if uses_io {
            let flushed = b.runtime(ctx, Runtime::IoClose, vec![]);
            let closed = b.new_block(ctx, vec![]);
            b.cond_br(ctx, flushed, (closed, vec![]), (wait, vec![]));
            b.block = closed;
        }
        b.return_resources(ctx);
        b.begin_resources(
            ctx,
            export.result.as_ref().is_some_and(AbiType::has_resources),
            None,
        )?;
        let mut flat = Vec::new();
        if let Some(outcome) = outcome {
            flat.push(outcome);
        } else if let (Some(ty), Some(mir_ty)) = (&export.result, mir_results.first()) {
            let value =
                b.load_slot_value(ctx, promise, perry_abi::layout::promise::VALUE, *mir_ty)?;
            if indirect {
                let size = b.i32_const(ctx, ty.size() as i32);
                let area = b.alloc(ctx, size);
                if let Some(completion) = completion {
                    b.completion_store(ctx, completion, 0, Some((*mir_ty, value)), area)?;
                } else {
                    b.lower_store(ctx, ty, *mir_ty, value, area, 0)?;
                }
                flat.push(area);
            } else {
                if let Some(completion) = completion {
                    flat = b.completion_flat(ctx, completion, 0, Some((*mir_ty, value)))?;
                } else {
                    b.lower_flat_values(ctx, ty, *mir_ty, value, &mut flat)?;
                }
            }
        }
        if b.state.uses_http_server {
            // Return the response once, then keep running until its body is written: the
            // host reads it only after the return.
            let returned = b.runtime(ctx, Runtime::WebServeReturned, vec![]);
            let first = b.new_block(ctx, vec![]);
            let after = b.new_block(ctx, vec![]);
            b.cond_br(ctx, returned, (after, vec![]), (first, vec![]));
            b.block = first;
            b.host_void(ctx, task_return.clone(), std::mem::take(&mut flat));
            b.runtime_void(ctx, Runtime::WebServeMarkReturned, vec![]);
            b.br(ctx, after, vec![]);
            b.block = after;
            let busy = b.runtime(ctx, Runtime::WebServeBusy, vec![]);
            let done = b.new_block(ctx, vec![]);
            b.cond_br(ctx, busy, (wait, vec![]), (done, vec![]));
            b.block = done;
            b.runtime_void(ctx, Runtime::WebServeClose, vec![]);
        }
        if b.state.uses_fetch {
            // No request or response handle outlives the export call.
            b.runtime_void(ctx, Runtime::WebFetchClose, vec![]);
        }
        if b.state.uses_streams {
            // No stream end outlives the export call.
            b.runtime_void(ctx, Runtime::WebStreamsClose, vec![]);
        }
        b.commit_resources(ctx);
        b.finish_resources(ctx);
        if !b.state.uses_http_server {
            b.host_void(ctx, task_return, flat);
        }
        b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
        b.task_free(ctx);
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
        let task = b.task_enter(ctx);
        b.host_void(ctx, EffectHost::ContextSet0, vec![task]);
        let initialization = b.initialization_status(ctx);
        let joined = initialization.map(|status| {
            let active = b.new_block(ctx, vec![]);
            let failed = b.new_block(ctx, vec![]);
            let joined = b.new_block(ctx, vec![i32]);
            b.cond_br(ctx, status, (failed, vec![]), (active, vec![]));
            b.block = failed;
            let promise = b.runtime(ctx, Runtime::PromiseNew, vec![]);
            b.runtime_void(ctx, Runtime::PromiseReject, vec![promise, status]);
            b.br(ctx, joined, vec![promise]);
            b.block = active;
            joined
        });
        let args = b.lift_parameters(ctx, &export.params, &mir_params, &mut flat)?;
        let ramp = CallOp::new(ctx, function.get_symbol_name(ctx), args, vec![i32]);
        let mut promise = b.result(ctx, ramp.get_operation());
        if let Some(joined) = joined {
            b.br(ctx, joined, vec![promise]);
            b.block = joined;
            promise = joined.deref(ctx).get_argument(0);
        }
        b.runtime_void(ctx, Runtime::TaskSetResult, vec![promise]);
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
        let task = b.host(ctx, ValueHost::ContextGet0, vec![]);
        b.runtime_void(ctx, Runtime::TaskResume, vec![task]);
        let cancellation = b.i32_const(ctx, 6);
        let cancelled = b.num(ctx, NumOpcode::I32Eq, vec![event, cancellation]);
        let abort = b.new_block(ctx, vec![]);
        let active = b.new_block(ctx, vec![]);
        b.cond_br(ctx, cancelled, (abort, vec![]), (active, vec![]));
        b.block = abort;
        b.abort_task(ctx)?;
        b.host_void(ctx, EffectHost::TaskCancel, vec![]);
        let exit = b.i32_const(ctx, EXIT);
        let ret = ReturnOp::new(ctx, vec![exit]).get_operation();
        b.insert(ctx, ret);
        b.block = active;
        let next = b.new_block(ctx, vec![]);
        let completion = b.runtime(ctx, Runtime::PromiseEvent, vec![event, handle]);
        let observe = b.new_block(ctx, vec![]);
        let resume = b.new_block(ctx, vec![]);
        let host_event = b.new_block(ctx, vec![]);
        b.cond_br(ctx, completion, (observe, vec![]), (host_event, vec![]));
        b.block = observe;
        let frame = b.runtime(ctx, Runtime::PromiseNext, vec![completion]);
        b.cond_br(ctx, frame, (resume, vec![]), (next, vec![]));
        b.block = resume;
        let call = CallOp::new(ctx, crate::async_split::RESUME, vec![frame], vec![]);
        b.insert(ctx, call.get_operation());
        b.br(ctx, observe, vec![]);
        b.block = host_event;
        if b.state.uses_streams {
            // Reads of `stream<u8>`s take their stream events.
            let handled = b.runtime(ctx, Runtime::WebEvent, vec![event, handle, subtask_state]);
            let unhandled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, handled, (next, vec![]), (unhandled, vec![]));
            b.block = unhandled;
        }
        if b.state.uses_http_server {
            // The served exchange takes its stream and future events.
            let handled = b.runtime(
                ctx,
                Runtime::WebServeEvent,
                vec![event, handle, subtask_state],
            );
            let unhandled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, handled, (next, vec![]), (unhandled, vec![]));
            b.block = unhandled;
        }
        if b.state.uses_fetch {
            // Fetches take their subtask, stream and future events.
            let handled = b.runtime(
                ctx,
                Runtime::WebFetchEvent,
                vec![event, handle, subtask_state],
            );
            let unhandled = b.new_block(ctx, vec![]);
            b.cond_br(ctx, handled, (next, vec![]), (unhandled, vec![]));
            b.block = unhandled;
        }
        if b.state.uses_fs {
            // `node:fs` operations take their own subtask, stream and future events.
            let handled = b.runtime(ctx, Runtime::FsEvent, vec![event, handle, subtask_state]);
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
        b.runtime_void(ctx, Runtime::TaskSubtaskEvent, vec![handle, subtask_state]);
        b.br(ctx, next, vec![]);
        b.block = other;
        if b.state.uses_io || b.state.reports_rejection {
            b.runtime(ctx, Runtime::IoEvent, vec![event, handle, subtask_state]);
        }
        b.br(ctx, next, vec![]);
        b.block = next;
        if b.state.uses_json_bodies {
            // Bodies an event finished reading for `json()` are parsed here, so that the
            // stream machinery does not link the JSON parser.
            b.runtime_void(ctx, Runtime::WebJsonSettle, vec![]);
        }
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
