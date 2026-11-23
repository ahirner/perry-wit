//! Resolve guest promise handlers and timer callbacks.
use super::*;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn apply_promise_handle_call(
        &mut self,
        s: &mut EvalState<'h>,
        ident: FuncIdentity,
        arg_tys: &[Ty],
        args: Option<&'h [Expr]>,
        byte_offset: u32,
    ) -> Result<bool> {
        let Some(view) = self.index.view(ident.template) else {
            return Ok(false);
        };
        match view.kind {
            TemplateKind::PromiseResolve => {
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
                if let Some(&value_loc) = self
                    .promise_handle_locs
                    .get(&(ident.template, ident.capture_owner))
                {
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
                }
                Ok(true)
            }
            TemplateKind::PromiseReject => Ok(true),
            _ => Ok(false),
        }
    }

    fn callback_target(
        &mut self,
        s: &mut EvalState<'h>,
        ident: FuncIdentity,
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<Option<(InstanceId, Ty, bool)>> {
        let Some(view) = self.index.view(ident.template) else {
            return Ok(None);
        };
        let is_promise_handle =
            self.apply_promise_handle_call(s, ident, arg_tys, None, byte_offset)?;
        self.check_callback_params(s.inst, view, arg_tys, byte_offset)?;
        let recv = recv_ty.filter(|_| view.uses_this).map(Receiver::Object);
        let call_args = if is_promise_handle {
            arg_tys.get(..1).unwrap_or(&[])
        } else {
            arg_tys
        };
        let target = self.request_instance(
            InstanceReq::new(ident.template, call_args, Some(s.inst))
                .receiver(recv)
                .owner(ident.capture_owner)
                .at(byte_offset),
        )?;
        self.propagate_writes(s.inst, target);
        let ret = self.read_loc(self.instances[target].return_loc, s.inst);
        Ok(Some((target, ret, recv.is_some())))
    }

    fn resolve_callback_call(
        &mut self,
        s: &mut EvalState<'h>,
        cb_ty: Ty,
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<Option<(CallbackCall, Ty)>> {
        if cb_ty == Ty::BOTTOM || arg_tys.contains(&Ty::BOTTOM) {
            return Ok(None);
        }
        if matches!(cb_ty, Ty::NEVER | Ty::NULL | Ty::VOID) {
            let helper = self.intern_helper(HelperSpec::ThrowTypeError, byte_offset);
            return Ok(Some((CallbackCall::ThrowTypeError(helper), Ty::NEVER)));
        }
        let dispatch_idents = if let Some(idents) = self.types.func_union_members(cb_ty) {
            Some(idents)
        } else {
            let (stripped, nullish) = self.types.strip_nullish(cb_ty);
            if nullish.is_some()
                && let TyKind::Func(ident) = self.types.kind(stripped)
            {
                Some(vec![ident])
            } else {
                None
            }
        };
        if let Some(idents) = dispatch_idents {
            for &a in arg_tys {
                self.invalidate_conservative_cell(a);
            }
            let mut arms = Vec::with_capacity(idents.len());
            let mut ret = Ty::BOTTOM;
            let mut needs_recv = false;
            for ident in idents {
                let Some((target, r, pass_receiver)) =
                    self.callback_target(s, ident, recv_ty, arg_tys, byte_offset)?
                else {
                    return Ok(None);
                };
                needs_recv |= pass_receiver;
                ret = self.types.join(ret, r);
                arms.push((ident, target));
            }
            let arms_span = self.intern_dispatch_arms(&arms);
            let arg_tuple = self.types.tuple(arg_tys);
            let helper = self.intern_helper(
                HelperSpec::Dispatcher {
                    callee_ty: cb_ty,
                    arms: arms_span,
                    arg_tys: arg_tuple,
                    recv_ty: needs_recv.then_some(recv_ty).flatten(),
                },
                byte_offset,
            );
            return Ok(Some((CallbackCall::Dispatch { helper, needs_recv }, ret)));
        }
        let TyKind::Func(ident) = self.types.kind(cb_ty) else {
            if !self.types.is_open(cb_ty) && cb_ty != Ty::DYNAMIC {
                return Err(self.annotate_expr_error(
                    s,
                    diag!(codes::TYPE_MISMATCH, "expression is not callable"),
                    byte_offset,
                ));
            }
            return Ok(None);
        };
        for &a in arg_tys {
            self.invalidate_conservative_cell(a);
        }
        let Some((target, ret, pass_receiver)) =
            self.callback_target(s, ident, recv_ty, arg_tys, byte_offset)?
        else {
            return Ok(None);
        };
        let pass_env = self.is_escaping_env(ident.template, s.inst);
        Ok(Some((
            CallbackCall::Direct {
                target,
                pass_env,
                pass_receiver,
            },
            ret,
        )))
    }

    pub(super) fn eval_new_promise(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
    ) -> Result<Ty> {
        let Some((resolve_tid, reject_tid)) = self.index.promise_templates_of(expr) else {
            return Err(Diagnostic::internal("missing promise handle templates"));
        };
        let owner = CaptureOwner::Instance(s.inst);
        let value_loc = match self.promise_handle_locs.get(&(resolve_tid, owner)).copied() {
            Some(loc) => loc,
            None => {
                let loc = self.alloc_loc(Ty::BOTTOM);
                self.promise_handle_locs.insert((resolve_tid, owner), loc);
                self.promise_handle_locs.insert((reject_tid, owner), loc);
                loc
            }
        };
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
        self.mark_escaping_env(s.inst, resolve_ty);
        self.mark_escaping_env(s.inst, reject_ty);

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
            let (pass_resolve, pass_reject) = match executor {
                CallbackCall::Direct { target, .. } => {
                    let view = self
                        .index
                        .view(self.instances[target].template)
                        .ok_or_else(|| Diagnostic::internal("missing promise executor template"))?;
                    let arity = if view.params.iter().any(|p| p.is_rest) {
                        2
                    } else {
                        view.params.len()
                    };
                    (arity >= 1, arity >= 2)
                }
                CallbackCall::Dispatch { .. } => (true, true),
                CallbackCall::ThrowTypeError(_) => (false, false),
            };
            self.record_site(
                s.inst,
                expr,
                SiteResolution::PromiseNew {
                    value_loc,
                    resolve_tid,
                    reject_tid,
                    executor,
                    pass_resolve,
                    pass_reject,
                },
            );
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
        let inner_raw = match self.types.kind(promise_ty) {
            TyKind::Promise(i) => i,
            TyKind::Cell {
                loc,
                kind: CellKind::Promise,
            } => self.read_loc(loc, s.inst),
            _ => Ty::OPEN,
        };
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

        let mut on_fulfilled: Option<(CallbackCall, Ty)> = None;
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
                        self.index.view(id.template).is_some_and(|v| {
                            v.params.is_empty()
                                && !matches!(
                                    v.kind,
                                    TemplateKind::PromiseResolve | TemplateKind::PromiseReject
                                )
                        })
                    });
                    if inner_raw != Ty::BOTTOM || takes_zero {
                        let cb_args: &[Ty] = if inner_raw == Ty::BOTTOM {
                            &[]
                        } else {
                            std::slice::from_ref(&inner_raw)
                        };
                        if let Some((cb_call, mut r)) =
                            self.resolve_callback_call(s, ft, None, cb_args, byte_offset)?
                        {
                            while let Some(u) = self.unwrap_promise_once(s.inst, r) {
                                r = u;
                            }
                            f_ret = r;
                            on_fulfilled = Some((cb_call, ft));
                        }
                    }
                }
                ReactionKind::Catch => {}
            }
        }

        let mut on_rejected: Option<(CallbackCall, Ty)> = None;
        let mut g_ret = Ty::BOTTOM;
        if let Some(gt) = g_present {
            self.check_escaping_func(s.inst, gt, byte_offset, s.current_span)?;
            let err_ty = self.types.named(self.ctx, "Error");
            if let Some((cb_call, mut r)) =
                self.resolve_callback_call(s, gt, None, &[err_ty], byte_offset)?
            {
                while let Some(u) = self.unwrap_promise_once(s.inst, r) {
                    r = u;
                }
                g_ret = r;
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

        let result = self.expr_cell_loc(s.inst, expr)?;
        self.write_loc(result, ret_settled);
        let pass_fulfilled = on_fulfilled.is_some_and(|(cb, _)| cb.needs_value());
        let pass_rejected = on_rejected.is_some_and(|(cb, _)| cb.needs_value());
        let helper = self.intern_helper(
            HelperSpec::PromiseReaction {
                kind,
                promise_ty,
                inner_ty: inner_raw,
                on_fulfilled,
                on_rejected,
                ret_ty: ret_settled,
            },
            byte_offset,
        );
        self.record_site(
            s.inst,
            expr,
            SiteResolution::PromiseReaction {
                result,
                helper,
                pass_first: if kind == ReactionKind::Catch {
                    pass_rejected
                } else {
                    pass_fulfilled
                },
                pass_second: kind == ReactionKind::Then && pass_rejected,
            },
        );

        Ok(self.types.cell(result, CellKind::Promise))
    }

    pub(super) fn eval_timer_start(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        name: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let is_immediate = name == "setImmediate";
        let repeating = name == "setInterval";
        let Some(cb_expr) = args.first() else {
            return Err(self.annotate_expr_error(
                s,
                diag!(codes::TYPE_MISMATCH, "{name} requires a callback function"),
                byte_offset,
            ));
        };
        let cb_ty = self.eval_expr(s, cb_expr)?;
        let has_delay = !is_immediate && args.len() >= 2;
        if has_delay && let Some(delay_expr) = args.get(1) {
            self.eval_expr(s, delay_expr)?;
        }
        let extra_start = if is_immediate { 1 } else { 2 };
        let extra_args = args.get(extra_start..).unwrap_or(&[]);
        let extra_arg_tys = self.eval_args(s, extra_args)?;
        let timer_ty = self.types.named(self.ctx, "__PerryTimer");
        if cb_ty == Ty::BOTTOM || extra_arg_tys.contains(&Ty::BOTTOM) {
            return Ok(timer_ty);
        }
        self.check_escaping_func(s.inst, cb_ty, byte_offset, s.current_span)?;
        if let Some((cb_call, _)) =
            self.resolve_callback_call(s, cb_ty, Some(timer_ty), &extra_arg_tys, byte_offset)?
        {
            let extra_tuple = self.types.tuple(&extra_arg_tys);
            let pass_cb = cb_call.needs_value();
            let helper = self.intern_helper(
                HelperSpec::TimerRunner {
                    repeating,
                    is_immediate,
                    cb_call,
                    cb_ty,
                    extra_arg_tys: extra_tuple,
                },
                byte_offset,
            );
            self.record_site(
                s.inst,
                expr,
                SiteResolution::TimerStart {
                    helper,
                    has_delay,
                    is_immediate,
                    pass_cb,
                },
            );
        }
        Ok(timer_ty)
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
