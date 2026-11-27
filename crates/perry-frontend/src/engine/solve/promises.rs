//! Resolve guest promise handlers and timer callbacks.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn eval_static_call(
        &mut self,
        s: &mut EvalState<'h>,
        method: crate::methods::StaticMethod,
        args: &'h [Expr],
    ) -> Result<Ty> {
        use crate::methods::StaticMethod::*;
        let args_ty = self.eval_args(s, args)?;
        let first = args_ty.first().copied().unwrap_or(Ty::VOID);
        let settled = match method {
            Resolve => self.awaited_type(s.inst, first),
            Reject => Ty::NEVER,
            Combine(combinator) => {
                let source = self.resolve_cell_read(s.inst, first);
                let element = match args {
                    [Expr::Array(items, _)] if items.is_empty() => Ty::VOID,
                    [_] if source == Ty::BOTTOM => return Ok(Ty::BOTTOM),
                    [_] => {
                        builtins::iterated_element(self.ctx, self.types, source).unwrap_or(Ty::OPEN)
                    }
                    _ => Ty::OPEN,
                };
                let value = self.awaited_type(s.inst, element);
                if value == Ty::BOTTOM {
                    return Ok(value);
                }
                let value = if value == Ty::NEVER { Ty::VOID } else { value };
                match combinator {
                    perry_mir::Combinator::All => self.types.array(value),
                    perry_mir::Combinator::AllSettled => {
                        let settled = self.types.generic(
                            crate::shapes::TypeName::new(self.ctx, builtins::SETTLED.to_owned()),
                            &[value],
                        );
                        self.types.array(settled)
                    }
                    perry_mir::Combinator::Race => value,
                }
            }
            Temporal(factory) => {
                return Ok(builtins::native_ty(self.ctx, self.types, factory.result()));
            }
            PerformanceNow => return Ok(Ty::NUMBER),
            Unsupported(_) => return Ok(Ty::OPEN),
        };
        Ok(self.types.promise(settled))
    }

    pub(super) fn apply_promise_handle_call(
        &mut self,
        s: &mut EvalState<'h>,
        ident: FuncIdentity,
        arg_tys: &[Ty],
        args: Option<&'h [Expr]>,
        byte_offset: u32,
    ) -> Result<bool> {
        let view = self.index.view(ident.template);
        match view.kind {
            TemplateKind::PromiseResolve(site) => {
                if let Some(&arg0) = arg_tys.first()
                    && self.types.is_promise_ty(arg0)
                {
                    return Err(self.annotate_expr_error(
                        s,
                        Diagnostic::unsupported(
                            "resolving a Promise with another Promise is not supported yet",
                        ),
                        byte_offset,
                    ));
                }
                let value_loc = SiteEntry::promise(&self.inst_sites, ident.capture_owner, site)?;
                let val = arg_tys.first().copied().unwrap_or(Ty::VOID);
                if val != Ty::BOTTOM {
                    self.check_escaping_func(s.inst, val, byte_offset, s.current_span)?;
                    let norm = self.types.normalize_for_storage(self.ctx, self.decls, val);
                    self.write_loc(value_loc, norm);
                    if let Some(arg0_expr) = args.and_then(|a| a.first()) {
                        let cur = self.read_loc(value_loc, s.inst);
                        self.record_coercion(s.inst, arg0_expr, val, cur);
                    }
                }
                Ok(true)
            }
            TemplateKind::PromiseReject(_) => Ok(true),
            _ => Ok(false),
        }
    }

    fn resolve_callback_call(
        &mut self,
        s: &mut EvalState<'h>,
        cb_ty: Ty,
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<Option<(CallTarget, Ty)>> {
        let (result, target) = self.resolve_invocation(
            s,
            cb_ty,
            recv_ty,
            arg_tys,
            calls::CallOrigin::Callback(byte_offset, &[]),
        )?;
        Ok(target.map(|target| (target, result)))
    }

    pub(super) fn eval_new_promise(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
    ) -> Result<Ty> {
        let owner = CaptureOwner::Instance(s.inst);
        let site = self
            .index
            .expressions
            .index(expr)
            .ok_or_else(|| Diagnostic::internal("missing promise constructor site"))?;
        let Some(SiteResolution::PromiseNew {
            value_loc,
            resolve_tid,
            reject_tid,
            ..
        }) = self.inst_sites[s.inst]
            .get(&site)
            .and_then(|entry| entry.resolution.as_ref())
        else {
            return Err(Diagnostic::internal(
                "missing promise constructor resolution",
            ));
        };
        let (value_loc, resolve_tid, reject_tid) = (*value_loc, *resolve_tid, *reject_tid);
        if !type_args.is_empty() {
            let targs = self.eval_type_args(s.inst, type_args);
            if let Some(&ta) = targs.first() {
                self.constrain_promise_loc(value_loc, ta);
            }
        }
        let resolve_ty = self.types.func(FuncIdentity {
            template: resolve_tid,
            capture_owner: owner,
        });
        let reject_ty = self.types.func(FuncIdentity {
            template: reject_tid,
            capture_owner: owner,
        });
        self.mark_escaping_env(resolve_ty);
        self.mark_escaping_env(reject_ty);

        let Some((exec_expr, tail)) = args.split_first() else {
            return Err(self.annotate_expr_error(
                s,
                diag!(
                    codes::TYPE_MISMATCH,
                    "new Promise requires an executor function"
                ),
                byte_offset,
            ));
        };
        let exec_ty = self.eval_expr(s, exec_expr)?;
        for extra in tail {
            self.eval_expr(s, extra)?;
        }
        let promise_cell = self.types.cell(value_loc, CellKind::Promise);
        if exec_ty == Ty::BOTTOM {
            return Ok(promise_cell);
        }
        if let Some((executor, _)) =
            self.resolve_callback_call(s, exec_ty, None, &[resolve_ty, reject_ty], byte_offset)?
        {
            let (pass_resolve, pass_reject) = match executor.callee() {
                Some(FunctionTarget::Direct(target) | FunctionTarget::Closure(target)) => {
                    let view = self.index.view(self.instances[target].template);
                    let arity = if view.params.iter().any(|p| p.is_rest) {
                        2
                    } else {
                        view.params.len()
                    };
                    (arity >= 1, arity >= 2)
                }
                Some(FunctionTarget::Dispatcher(_)) => (true, true),
                None => (false, false),
            };
            let entry = self.inst_sites[s.inst]
                .get_mut(&site)
                .ok_or_else(|| Diagnostic::internal("missing promise constructor site"))?;
            entry.resolution = Some(SiteResolution::PromiseNew {
                value_loc,
                resolve_tid,
                reject_tid,
                executor: Some(PromiseExecutor {
                    call: executor,
                    handles: [
                        pass_resolve.then_some(resolve_tid),
                        pass_reject.then_some(reject_tid),
                    ]
                    .map(|tid| {
                        tid.map(|template| FuncIdentity {
                            template,
                            capture_owner: owner,
                        })
                    }),
                }),
            });
        }
        Ok(promise_cell)
    }

    pub(super) fn eval_promise_reaction(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        promise_ty: Ty,
        name: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let kind = match name {
            "then" => ReactionKind::Then,
            "catch" => ReactionKind::Catch,
            _ => ReactionKind::Finally,
        };
        let inner_raw = self.awaited_type(s.inst, promise_ty);
        let (fulfilled_expr, rejected_expr) = match kind {
            ReactionKind::Then => (args.first(), args.get(1)),
            ReactionKind::Catch => (None, args.first()),
            ReactionKind::Finally => (args.first(), None),
        };
        let f_ty = match fulfilled_expr {
            Some(e) => Some(self.eval_expr(s, e)?),
            None => None,
        };
        let g_ty = match rejected_expr {
            Some(e) => Some(self.eval_expr(s, e)?),
            None => None,
        };
        let extra_start = match kind {
            ReactionKind::Then => 2,
            ReactionKind::Catch | ReactionKind::Finally => 1,
        };
        for extra in args.get(extra_start..).unwrap_or(&[]) {
            self.eval_expr(s, extra)?;
        }
        if f_ty == Some(Ty::BOTTOM) || g_ty == Some(Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        let is_handler = |ty: Ty| self.types.has_func(ty) || self.types.is_open(ty);
        let f_present = f_ty.filter(|&t| is_handler(t));
        let g_present = g_ty.filter(|&t| is_handler(t));

        let mut on_fulfilled: Option<(CallTarget, Ty)> = None;
        let mut f_ret = Ty::BOTTOM;
        if let Some(ft) = f_present {
            self.check_escaping_func(s.inst, ft, byte_offset, s.current_span)?;
            match kind {
                ReactionKind::Finally => {
                    if let Some((cb_call, _)) =
                        self.resolve_callback_call(s, ft, None, &[], byte_offset)?
                    {
                        on_fulfilled = Some((cb_call, ft));
                    }
                }
                ReactionKind::Then => {
                    let takes_zero = self.types.func_members(&ft).all(|id| {
                        let v = self.index.view(id.template);
                        v.params.is_empty()
                            && !matches!(
                                v.kind,
                                TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_)
                            )
                    });
                    if inner_raw != Ty::BOTTOM || takes_zero {
                        let cb_args: &[Ty] = if inner_raw == Ty::BOTTOM {
                            &[]
                        } else {
                            std::slice::from_ref(&inner_raw)
                        };
                        if let Some((cb_call, r)) =
                            self.resolve_callback_call(s, ft, None, cb_args, byte_offset)?
                        {
                            f_ret = self.awaited_type(s.inst, r);
                            on_fulfilled = Some((cb_call, ft));
                        }
                    }
                }
                ReactionKind::Catch => {}
            }
        }

        let mut on_rejected: Option<(CallTarget, Ty)> = None;
        let mut g_ret = Ty::BOTTOM;
        if let Some(gt) = g_present {
            self.check_escaping_func(s.inst, gt, byte_offset, s.current_span)?;
            let err_ty = self.types.named(self.ctx, "Error");
            if let Some((cb_call, r)) =
                self.resolve_callback_call(s, gt, None, &[err_ty], byte_offset)?
            {
                g_ret = self.awaited_type(s.inst, r);
                on_rejected = Some((cb_call, gt));
            }
        }

        let ret_settled = match kind {
            ReactionKind::Then => {
                let ful = match f_present {
                    Some(ty) if self.types.strip_nullish(ty).1.is_some() => {
                        self.types.join(inner_raw, f_ret)
                    }
                    Some(_) => f_ret,
                    None => inner_raw,
                };
                if g_present.is_some() {
                    self.types.join(ful, g_ret)
                } else {
                    ful
                }
            }
            ReactionKind::Catch => {
                if g_present.is_some() {
                    self.types.join(inner_raw, g_ret)
                } else {
                    inner_raw
                }
            }
            ReactionKind::Finally => inner_raw,
        };

        let helper = self.intern_helper(
            HelperSpec::PromiseReaction(PromiseReaction {
                kind,
                promise_ty,
                inner_ty: inner_raw,
                on_fulfilled,
                on_rejected,
                ret_ty: ret_settled,
            }),
            byte_offset,
        );
        let (site, result, cell) = self.storage_site(s.inst, expr, CellKind::Promise)?;
        site.resolution = Some(SiteResolution::PromiseReaction { result, helper });
        self.write_loc(result, ret_settled);
        Ok(cell)
    }

    pub(super) fn eval_timer_start(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        name: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let mut inputs = self.eval_args(s, args)?;
        let Some((&mut cb_ty, tail)) = inputs.split_first_mut() else {
            return Err(self.annotate_expr_error(
                s,
                diag!(codes::TYPE_MISMATCH, "{name} requires a callback function"),
                byte_offset,
            ));
        };
        let schedule = crate::engine::TimerSchedule {
            repeats: name == "setInterval",
            has_delay: name != "setImmediate" && !tail.is_empty(),
        };
        let extras = if schedule.has_delay
            && let Some((delay, extras)) = tail.split_first_mut()
        {
            *delay = Ty::NUMBER;
            extras
        } else {
            tail
        };
        if cb_ty == Ty::BOTTOM || extras.contains(&Ty::BOTTOM) {
            return Ok(Ty::TIMER);
        }
        self.check_escaping_func(s.inst, cb_ty, byte_offset, s.current_span)?;
        if let Some((cb_call, _)) =
            self.resolve_callback_call(s, cb_ty, Some(Ty::TIMER), extras, byte_offset)?
        {
            let inputs = self.types.tuple(&inputs);
            let helper = self.intern_helper(
                HelperSpec::TimerRunner {
                    schedule,
                    cb_call,
                    inputs,
                },
                byte_offset,
            );
            self.record_site(s.inst, expr, SiteResolution::TimerStart { helper });
        }
        Ok(Ty::TIMER)
    }

    pub(super) fn eval_timer_clear(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        args: &'h [Expr],
    ) -> Result<Ty> {
        let arg_tys = self.eval_args(s, args)?;
        self.record_site(
            s.inst,
            expr,
            SiteResolution::TimerClear {
                arg_ty: arg_tys.first().copied(),
            },
        );
        Ok(Ty::VOID)
    }
}
