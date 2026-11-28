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
    ensure_diag!(
        is_async == export.is_async(),
        codes::ASYNC_MISMATCH,
        "{} is {}async but the WIT export is {}async",
        export.declaration(),
        if is_async { "" } else { "not " },
        if export.is_async() { "" } else { "not " },
    );
    let (mir_params, mir_results) = crate::analysis::function_signature(ctx, function)?;
    ensure_diag!(
        mir_params.len() == export.params.len(),
        codes::CONTRACT_MISMATCH,
        "{} takes {} parameters but the WIT export has {}",
        export.declaration(),
        mir_params.len(),
        export.params.len()
    );
    let completion =
        if let Some(payloads) = export.result.as_deref().and_then(AbiType::result_payloads) {
            let layout = mir_results
                .first()
                .and_then(|&ty| perry_mir::typed::<RecordType>(ctx, ty))
                .map(|record| state.record_layouts.get(ctx, record))
                .transpose()?;
            !layout
                .and_then(|layout| sums::TaggedResult::of(ctx, layout))
                .is_some_and(|result| {
                    perry_mir::RESULT_PAYLOADS
                        .into_iter()
                        .zip(payloads)
                        .all(|(name, payload)| {
                            payload.is_none() || result.layout.fields.contains_key(name)
                        })
                })
        } else {
            false
        };
    if let ExportKind::Async {
        task_return,
        completion: mode,
    } = &export.kind
    {
        return async_export(
            ctx,
            state,
            function,
            export,
            (task_return, *mode),
            (&mir_params, &mir_results),
            completion,
        );
    }
    if !completion && mir_results.len() != usize::from(export.result.is_some()) {
        return Err(diag!(
            codes::CONTRACT_MISMATCH,
            "{} returns {} values but the WIT export returns {}",
            export.declaration(),
            mir_results.len(),
            usize::from(export.result.is_some())
        )
        .with_fallback_span(
            export
                .annotation(export.params.len())
                .or_else(|| perry_mir::source_span(ctx, function)),
        ));
    }
    let indirect = export.signature.retptr;
    let params = core_types(ctx, &export.signature.params);
    let results: Vec<_> = core_types(ctx, &export.signature.results);
    let post_ty = FunctionType::get(ctx, results.clone(), vec![]);
    let fn_ty = FunctionType::get(ctx, params, results);
    let glue = FuncOp::new(ctx, identifier(&export.core_name), fn_ty);
    set_export(ctx, &glue, &export.core_name);
    let entry = glue.get_entry_block(ctx);
    let flat: Vec<Value> = entry.deref(ctx).arguments().collect();

    let mut b = FunctionLowering::glue(state, ctx, &glue);
    if scope == Scope::Task {
        let task = b.task_enter(ctx);
        if b.state.p2 && b.state.uses_io {
            b.host_void(ctx, EffectHost::ContextSet0, vec![task]);
        }
    }
    let initialization = b.initialization_status(ctx);
    let mut call_results = mir_results
        .iter()
        .map(|&ty| wasm_type(ctx, ty))
        .collect::<Result<Vec<_>>>()?;
    let joined = initialization.map(|status| {
        let active = b.new_block(ctx, vec![]);
        let failed = b.new_block(ctx, vec![]);
        let mut values: Vec<_> = call_results.iter().map(|&ty| b.zero(ctx, ty)).collect();
        values.push(status);
        let mut joined_types = call_results.clone();
        joined_types.push(i32_type(ctx));
        let joined = b.new_block(ctx, joined_types);
        b.cond_br(ctx, status, (failed, vec![]), (active, vec![]));
        b.block = failed;
        b.br(ctx, joined, values);
        b.block = active;
        joined
    });
    let args = b.lift_parameters(ctx, export, &mir_params, &flat)?;
    let throws = b.state.may_throw(&function.get_symbol_name(ctx));
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
        let normal = b.enter_if(ctx, status);
        b.return_resources(ctx);
        if let Some(ty) = export.exception_result() {
            let returned = b.lower_exception(ctx, ty, status, indirect)?;
            b.ret(ctx, returned);
        } else {
            b.task_free(ctx);
            b.trap(ctx);
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
        export.result.as_deref().is_some_and(AbiType::has_resources),
        None,
    )?;
    let payload = mir_results
        .first()
        .map(|ty| (*ty, Carriers::Scalar(values[0])));
    let source = if completion {
        Some(BoundaryValue::Case(&["ok"], payload))
    } else {
        payload.map(|(mir, value)| BoundaryValue::Native(mir, value))
    };
    let returned = match (&export.result, source) {
        (Some(ty), Some(source)) => b.lower_return(ctx, ty, source, indirect).map_err(|e| {
            e.with_fallback_span(
                export
                    .annotation(export.params.len())
                    .or_else(|| perry_mir::source_span(ctx, function)),
            )
        })?,
        _ => vec![],
    };
    b.commit_resources(ctx);
    b.finish_resources(ctx);
    if scope == Scope::Task && b.state.p2 && b.state.uses_io {
        p2_effect(&mut b, ctx, "p2_io_flush", vec![]);
    }
    b.ret(ctx, returned);
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
    if b.state.p2 && b.state.uses_io {
        p2_effect(&mut b, ctx, "p2_close", vec![]);
    }
    b.task_free(ctx);
    b.ret(ctx, vec![]);
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
    ((module, wit_name), mode): (&(String, String), AsyncCompletion),
    (mir_params, mir_results): (&[TypeHandle], &[TypeHandle]),
    completion: bool,
) -> Result<()> {
    // `task.return` takes the result flat, or through memory beyond MAX_FLAT_PARAMS.
    let command = matches!(mode, AsyncCompletion::Command);
    let result = if command {
        Some([WasmType::I32].as_slice())
    } else {
        export
            .result
            .as_ref()
            .map_or(Some(&[][..]), |ty| ty.flat.as_deref())
    };
    let blocking = state.p2;
    let indirect = if blocking {
        export.signature.retptr
    } else {
        result.is_none()
    };
    let returned_type = export.signature.results.first().copied();
    let return_slot = if blocking {
        state.data.append(8, &[0; 8])
    } else {
        0
    };
    let task_return = EffectHost::TaskReturn(
        module.clone(),
        wit_name.clone(),
        result.unwrap_or(&[WasmType::I32]).to_vec(),
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
        let mut promise = if command {
            let mut pending = b.i32_const(ctx, 0);
            for (used, check) in [
                (b.state.uses_fetch, Runtime::WebFetchPending),
                (b.state.uses_streams, Runtime::WebStreamsPending),
                (b.state.uses_fs, Runtime::FsPending),
            ] {
                if used {
                    let active = b.runtime(ctx, check, vec![]);
                    pending = b.num(ctx, NumOpcode::I32Or, vec![pending, active]);
                }
            }
            b.runtime(ctx, Runtime::CommandResult, vec![pending])
        } else {
            b.runtime(ctx, Runtime::TaskResult, vec![])
        };
        let ready = if command {
            promise
        } else {
            b.load(
                ctx,
                MemKind::I32,
                promise,
                perry_abi::layout::promise::STATE,
            )
        };
        let wait = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.cond_br(ctx, ready, (done, vec![]), (wait, vec![]));

        // Pending: leave the task and wait on its subtasks (none would wait forever).
        b.block = wait;
        let set = b.runtime(ctx, Runtime::TaskSuspend, vec![]);
        b.trap_unless(ctx, set, false);
        let four = b.i32_const(ctx, 4);
        let shifted = b.num(ctx, NumOpcode::I32Shl, vec![set, four]);
        let wait_code = b.i32_const(ctx, WAIT);
        let code = b.num(ctx, NumOpcode::I32Or, vec![shifted, wait_code]);
        b.ret(ctx, vec![code]);

        // Settled: a rejection without a WIT error type traps (a command reports it and
        // fails); otherwise return the value once the output reached the host.
        b.block = done;
        // Propagate combinator cancellation ownership before checking helper-owned work.
        let mut detached = b.runtime(ctx, Runtime::TaskCancel, vec![]);
        b.cleanup_foreign(ctx)?;
        if b.state.uses_fs {
            let filesystem = b.runtime(ctx, Runtime::FsClose, vec![]);
            if !command {
                detached = b.num(ctx, NumOpcode::I32Add, vec![detached, filesystem]);
            }
        }
        let outcome = if command {
            Some(b.runtime(ctx, Runtime::CommandOutcome, vec![promise]))
        } else {
            let mut settled = ready;
            for (used, check) in [
                (b.state.uses_fetch, Runtime::WebFetchPending),
                (b.state.uses_streams, Runtime::WebStreamsPending),
            ] {
                if used {
                    let count = b.runtime(ctx, check, vec![]);
                    detached = b.num(ctx, NumOpcode::I32Add, vec![detached, count]);
                }
            }
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
            let rejected_state = b.i32_const(ctx, perry_abi::layout::promise::REJECTED as i32);
            let rejected = b.num(ctx, NumOpcode::I32Eq, vec![settled, rejected_state]);
            let fulfilled = b.enter_if(ctx, rejected);
            b.return_resources(ctx);
            if let Some(ty) = export.exception_result() {
                if uses_io {
                    b.flush_io(ctx, wait);
                }
                let values = if let AsyncCompletion::RejectionText(path) = mode {
                    let text = b.runtime(ctx, Runtime::RejectionText, vec![promise]);
                    let payload = Some((
                        perry_mir::StringType::get(ctx).into(),
                        Carriers::Scalar(text),
                    ));
                    b.lower_return(ctx, ty, BoundaryValue::Case(path, payload), indirect)?
                } else {
                    let status = b.runtime(ctx, Runtime::PromiseRethrow, vec![promise]);
                    b.lower_exception(ctx, ty, status, indirect)?
                };
                b.close_web_handles(ctx, true);
                return_result(
                    &mut b,
                    ctx,
                    task_return.clone(),
                    blocking,
                    return_slot,
                    returned_type,
                    values,
                );
                b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
                if !blocking {
                    b.task_free(ctx);
                }
                let exit = b.i32_const(ctx, EXIT);
                b.ret(ctx, vec![exit]);
            } else {
                if uses_io || b.state.reports_rejection {
                    b.runtime_void(ctx, Runtime::ReportRejection, vec![promise]);
                    b.flush_io(ctx, wait);
                }
                b.close_web_handles(ctx, true);
                b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
                b.return_resources(ctx);
                b.task_free(ctx);
                b.trap(ctx);
            }
            b.block = fulfilled;
            None
        };
        if uses_io {
            b.flush_io(ctx, wait);
        }
        b.return_resources(ctx);
        b.begin_resources(
            ctx,
            export.result.as_deref().is_some_and(AbiType::has_resources),
            None,
        )?;
        let mut flat = Vec::new();
        if let Some(outcome) = outcome {
            flat.push(outcome);
        } else if let (Some(ty), Some(mir_ty)) = (&export.result, mir_results.first()) {
            let value =
                b.load_slot_value(ctx, promise, perry_abi::layout::promise::VALUE, *mir_ty)?;
            let source = if completion {
                BoundaryValue::Case(&["ok"], Some((*mir_ty, value)))
            } else {
                BoundaryValue::Native(*mir_ty, value)
            };
            flat = b.lower_return(ctx, ty, source, indirect).map_err(|error| {
                error.with_fallback_span(
                    export
                        .annotation(export.params.len())
                        .or_else(|| perry_mir::source_span(ctx, function)),
                )
            })?;
        }
        if b.state.uses_http_server {
            // Return the response once, then keep running until its body is written: the
            // host reads it only after the return.
            let returned = b.runtime(ctx, Runtime::WebServeReturned, vec![]);
            let first = b.new_block(ctx, vec![]);
            let after = b.new_block(ctx, vec![]);
            b.cond_br(ctx, returned, (after, vec![]), (first, vec![]));
            b.block = first;
            return_result(
                &mut b,
                ctx,
                task_return.clone(),
                blocking,
                return_slot,
                returned_type,
                std::mem::take(&mut flat),
            );
            b.runtime_void(ctx, Runtime::WebServeMarkReturned, vec![]);
            b.br(ctx, after, vec![]);
            b.block = after;
            let busy = b.runtime(ctx, Runtime::WebServeBusy, vec![]);
            let done = b.new_block(ctx, vec![]);
            b.cond_br(ctx, busy, (wait, vec![]), (done, vec![]));
            b.block = done;
            b.runtime_void(ctx, Runtime::WebServeClose, vec![]);
        }
        b.close_web_handles(ctx, false);
        b.commit_resources(ctx);
        b.finish_resources(ctx);
        if !b.state.uses_http_server {
            return_result(
                &mut b,
                ctx,
                task_return,
                blocking,
                return_slot,
                returned_type,
                flat,
            );
        }
        b.runtime_void(ctx, Runtime::TaskCloseSet, vec![]);
        if !blocking {
            b.task_free(ctx);
        }
        let exit = b.i32_const(ctx, EXIT);
        b.ret(ctx, vec![exit]);
        b.state
            .wasm
            .append_operation(ctx, finish.get_operation(), 0);
    }

    // [async-lift]<export>(params) -> code
    let lift_name = format!("[async-lift]{}", export.core_name);
    let params = core_types(ctx, &export.signature.params);
    let lift_ty = FunctionType::get(ctx, params, vec![i32]);
    let lift = FuncOp::new(ctx, identifier(&lift_name), lift_ty);
    if !blocking {
        set_export(ctx, &lift, &lift_name);
    }
    {
        let flat: Vec<Value> = lift.get_entry_block(ctx).deref(ctx).arguments().collect();
        let mut b = FunctionLowering::glue(state, ctx, &lift);
        let task = b.task_enter(ctx);
        if command {
            b.runtime_void(ctx, Runtime::CommandStart, vec![]);
        }
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
        let args = b.lift_parameters(ctx, export, mir_params, &flat)?;
        let ramp = CallOp::new(ctx, function.get_symbol_name(ctx), args, vec![i32]);
        let mut promise = b.result(ctx, ramp.get_operation());
        if let Some(joined) = joined {
            promise = b.join_value(ctx, joined, promise);
        }
        b.runtime_void(ctx, Runtime::TaskSetResult, vec![promise]);
        let call = CallOp::new(ctx, finish_name.clone(), vec![], vec![i32]);
        let code = b.result(ctx, call.get_operation());
        b.ret(ctx, vec![code]);
        b.state.wasm.append_operation(ctx, lift.get_operation(), 0);
    }

    // [callback][async-lift]<export>(event, p1, p2) -> code
    let callback_name = format!("[callback]{lift_name}");
    let callback_ty = FunctionType::get(ctx, vec![i32; 3], vec![i32]);
    let callback = FuncOp::new(ctx, identifier(&callback_name), callback_ty);
    if !blocking {
        set_export(ctx, &callback, &callback_name);
    }
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
        let active = b.enter_if(ctx, cancelled);
        b.abort_task(ctx)?;
        b.host_void(ctx, EffectHost::TaskCancel, vec![]);
        let exit = b.i32_const(ctx, EXIT);
        b.ret(ctx, vec![exit]);
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
        for (used, handler) in [
            (b.state.uses_streams, Runtime::WebEvent),
            (b.state.uses_http_server, Runtime::WebServeEvent),
            (b.state.uses_fetch, Runtime::WebFetchEvent),
            (b.state.uses_fs, Runtime::FsEvent),
        ] {
            if used {
                let handled = b.runtime(ctx, handler, vec![event, handle, subtask_state]);
                let unhandled = b.new_block(ctx, vec![]);
                b.cond_br(ctx, handled, (next, vec![]), (unhandled, vec![]));
                b.block = unhandled;
            }
        }
        let subtask_event = b.i32_const(ctx, EVENT_SUBTASK);
        let is_subtask = b.num(ctx, NumOpcode::I32Eq, vec![event, subtask_event]);
        let other = b.enter_if(ctx, is_subtask);
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
        b.ret(ctx, vec![code]);
        b.state
            .wasm
            .append_operation(ctx, callback.get_operation(), 0);
    }
    if blocking {
        let params = core_types(ctx, &export.signature.params);
        let results: Vec<_> = returned_type
            .into_iter()
            .map(|f| flat_type(ctx, f))
            .collect();
        let ty = FunctionType::get(ctx, params, results.clone());
        let wrapper = FuncOp::new(ctx, identifier(&export.core_name), ty);
        set_export(ctx, &wrapper, &export.core_name);
        let mut b = FunctionLowering::glue(state, ctx, &wrapper);
        let args = wrapper
            .get_entry_block(ctx)
            .deref(ctx)
            .arguments()
            .collect();
        let call = CallOp::new(ctx, identifier(&lift_name), args, vec![i32]);
        let code = b.result(ctx, call.get_operation());
        let waiting = b.new_block(ctx, vec![i32]);
        let poll = b.new_block(ctx, vec![]);
        let done = b.new_block(ctx, vec![]);
        b.br(ctx, waiting, vec![code]);
        b.block = waiting;
        let code = waiting.deref(ctx).get_argument(0);
        b.cond_br(ctx, code, (poll, vec![]), (done, vec![]));
        b.block = poll;
        let four = b.i32_const(ctx, 4);
        let set = b.num(ctx, NumOpcode::I32ShrU, vec![code, four]);
        let area = p2_value(&mut b, ctx, "p2_poll", vec![set]);
        let args = [0, 4, 8]
            .into_iter()
            .map(|offset| b.load(ctx, MemKind::I32, area, offset))
            .collect();
        let call = CallOp::new(ctx, identifier(&callback_name), args, vec![i32]);
        let next = b.result(ctx, call.get_operation());
        b.br(ctx, waiting, vec![next]);
        b.block = done;
        let returned = returned_type
            .map(|ty| {
                let area = b.i32_const(ctx, return_slot as i32);
                b.load(ctx, return_mem_kind(ty), area, 0)
            })
            .into_iter()
            .collect();
        b.ret(ctx, returned);
        b.state
            .wasm
            .append_operation(ctx, wrapper.get_operation(), 0);

        // Synchronous canonical lifting reads strings/lists before post-return frees them.
        let name = format!("cabi_post_{}", export.core_name);
        let post_ty = FunctionType::get(ctx, results, vec![]);
        let post = FuncOp::new(ctx, identifier(&name), post_ty);
        set_export(ctx, &post, &name);
        let mut b = FunctionLowering::glue(state, ctx, &post);
        p2_effect(&mut b, ctx, "p2_close", vec![]);
        b.task_free(ctx);
        b.ret(ctx, vec![]);
        b.state.wasm.append_operation(ctx, post.get_operation(), 0);
    }
    Ok(())
}

fn p2_value(
    b: &mut FunctionLowering<'_, '_>,
    ctx: &mut Context,
    name: &str,
    args: Vec<Value>,
) -> Value {
    b.host(
        ctx,
        ValueHost::Foreign(
            crate::host::Foreign {
                module: crate::HELPER_MODULE.into(),
                name: name.into(),
                params: vec![WasmType::I32; args.len()],
            },
            WasmType::I32,
        ),
        args,
    )
}

fn p2_effect(b: &mut FunctionLowering<'_, '_>, ctx: &mut Context, name: &str, args: Vec<Value>) {
    b.host_void(
        ctx,
        EffectHost::Foreign(crate::host::Foreign {
            module: crate::HELPER_MODULE.into(),
            name: name.into(),
            params: vec![WasmType::I32; args.len()],
        }),
        args,
    );
}

fn return_mem_kind(ty: WasmType) -> MemKind {
    match ty {
        WasmType::I32 | WasmType::Pointer | WasmType::Length => MemKind::I32,
        WasmType::I64 | WasmType::PointerOrI64 => MemKind::I64,
        WasmType::F32 => MemKind::F32,
        WasmType::F64 => MemKind::F64,
    }
}

fn return_result(
    b: &mut FunctionLowering<'_, '_>,
    ctx: &mut Context,
    host: EffectHost,
    blocking: bool,
    slot: u32,
    ty: Option<WasmType>,
    values: Vec<Value>,
) {
    if blocking {
        if let Some(ty) = ty {
            let address = b.i32_const(ctx, slot as i32);
            b.store(ctx, return_mem_kind(ty), address, 0, values[0]);
        }
    } else {
        b.host_void(ctx, host, values);
    }
}

impl FunctionLowering<'_, '_> {
    fn flush_io(
        &mut self,
        ctx: &mut Context,
        wait: pliron::context::Ptr<pliron::basic_block::BasicBlock>,
    ) {
        let flushed = self.runtime(ctx, Runtime::IoClose, vec![]);
        let closed = self.new_block(ctx, vec![]);
        self.cond_br(ctx, flushed, (closed, vec![]), (wait, vec![]));
        self.block = closed;
    }

    fn lower_exception(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        status: Value,
        indirect: bool,
    ) -> Result<Vec<Value>> {
        let exception = self.runtime(ctx, Runtime::ExceptionCatch, vec![status]);
        let payload = self.completion_error(ctx, ty, exception)?;
        self.lower_return(ctx, ty, BoundaryValue::Case(&["err"], payload), indirect)
    }

    fn lower_return(
        &mut self,
        ctx: &mut Context,
        ty: &AbiType,
        source: BoundaryValue,
        indirect: bool,
    ) -> Result<Vec<Value>> {
        let mut flat = Vec::new();
        let area = if indirect {
            let size = self.i32_const(ctx, ty.size as i32);
            Some(self.alloc(ctx, size))
        } else {
            None
        };
        let target = match area {
            Some(area) => Canonical::Memory(area, 0),
            None => Canonical::Flat(&mut flat),
        };
        self.lower_abi(ctx, ty, source, target)?;
        if let Some(area) = area {
            flat.push(area);
        }
        Ok(flat)
    }
}
