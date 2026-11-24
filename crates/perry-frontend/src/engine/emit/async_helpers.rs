//! Emission of guest-settled promises (`new Promise`, `then`/`catch`/`finally`) and global timers.
use super::{Emitter, RewriteCtx, is_pure_func_expr, sequence};
use crate::engine::boundary::{TemplateKind, TemplateView};
use crate::engine::solve::{CallbackCall, HelperSpec, Instance, ReactionKind, SiteEntry};
use crate::engine::types::{CaptureOwner, FuncIdentity, HelperId, LocId, TemplateId, Ty};
use crate::hir_compat::GuestCall;
use perry_hir::{
    ir::{CatchClause, Expr, Function, Stmt, UnaryOp},
    types::{FuncId, LocalId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result};

const TIMER_CLASS: &str = "__PerryTimer";

impl<'h> Emitter<'h, '_> {
    pub(super) fn inline_iife(&mut self, return_type: HirType, body: Vec<Stmt>) -> Result<Expr> {
        let closure_fid = self.fresh_func_id()?;
        let closure = Expr::Closure {
            func_id: closure_fid,
            params: Vec::new(),
            return_type,
            body,
            captures: Vec::new(),
            mutable_captures: Vec::new(),
            captures_this: false,
            captures_new_target: false,
            enclosing_class: None,
            is_async: false,
            is_generator: false,
            is_strict: true,
            construct_only: false,
            is_arrow: true,
        };
        Ok(Expr::Call {
            callee: Box::new(closure),
            args: Vec::new(),
            type_args: Vec::new(),
            byte_offset: 0,
            byte_end: 0,
        })
    }

    fn promise_inner_hir(&mut self, value_loc: LocId) -> Result<HirType> {
        let elem = self.solved.locations[value_loc];
        if elem == Ty::BOTTOM {
            Ok(HirType::Void)
        } else {
            self.materialize(elem)
        }
    }

    pub(super) fn emit_promise_env_class(
        &mut self,
        ident: FuncIdentity,
        name: String,
    ) -> Result<String> {
        let Some(TemplateKind::PromiseResolve(site) | TemplateKind::PromiseReject(site)) =
            self.index.view(ident.template).map(|v| v.kind)
        else {
            return Err(Diagnostic::internal("missing promise handle template"));
        };
        let value_loc = SiteEntry::promise(&self.solved.inst_sites, ident.capture_owner, site)?;
        let prom_ty = HirType::Promise(Box::new(self.promise_inner_hir(value_loc)?));
        self.class_with_fields(name, [("c0".to_owned(), prom_ty)])
    }

    pub(super) fn emit_promise_handle_function(
        &mut self,
        inst: &Instance,
        view: &TemplateView<'h>,
    ) -> Result<Function> {
        let self_ident = FuncIdentity {
            template: inst.template,
            capture_owner: inst.capture_owner,
        };
        let env_class = self.ensure_env_class(self_ident)?;
        let env_pid = self.fresh_synthetic_local()?;
        let mut params = vec![Self::synth_param(
            env_pid,
            "__env".to_owned(),
            HirType::Named(env_class),
        )];
        let val_expr = match self.types.tuple_slice(inst.args) {
            [arg_ty] => {
                let val_pid = self.fresh_synthetic_local()?;
                let arg_hir = self.materialize(*arg_ty)?;
                params.push(Self::synth_param(val_pid, "__v".to_owned(), arg_hir));
                Expr::LocalGet(val_pid)
            }
            _ => Expr::Undefined,
        };
        let promise = Self::prop_get(Expr::LocalGet(env_pid), "c0");
        let call = if matches!(view.kind, TemplateKind::PromiseResolve(_)) {
            GuestCall::Resolve {
                promise,
                value: val_expr,
            }
        } else {
            GuestCall::Reject {
                promise,
                reason: val_expr,
            }
        };
        let body = vec![Stmt::Expr(call.into_hir())];
        Ok(Self::synth_function(
            inst.emitted_func_id,
            inst.emitted_name.clone(),
            params,
            HirType::Void,
            body,
            false,
        ))
    }

    pub(super) fn ensure_timer_class(&mut self) -> Result<String> {
        if self.emitted_timer_class {
            return Ok(TIMER_CLASS.to_owned());
        }
        self.emitted_timer_class = true;
        self.class_with_fields(
            TIMER_CLASS.to_owned(),
            [
                (
                    "ctrl".to_owned(),
                    HirType::Named("AbortController".to_owned()),
                ),
                ("cancelled".to_owned(), HirType::Boolean),
            ],
        )
    }

    fn emit_callback_call_expr(
        &mut self,
        cb: CallbackCall,
        cb_val: Option<Expr>,
        recv: Option<Expr>,
        args: Vec<Expr>,
    ) -> Result<(Expr, bool)> {
        match cb {
            CallbackCall::ThrowTypeError(helper) => {
                let fid = self.emit_helper(helper)?;
                Ok((Self::synth_call(fid, Vec::new(), 0, 0), false))
            }
            CallbackCall::Direct {
                target,
                pass_env,
                pass_receiver,
            } => {
                let (fid, template, target_args, ret_loc) = {
                    let ti = self.instance(target, "missing callback call target instance")?;
                    (ti.emitted_func_id, ti.template, ti.args, ti.return_loc)
                };
                let view = self
                    .index
                    .view(template)
                    .ok_or_else(|| Diagnostic::internal("missing callback target template"))?;
                let mut call_args = Vec::with_capacity(args.len() + 2);
                if pass_env {
                    let val = cb_val.ok_or_else(|| {
                        Diagnostic::internal("missing environment callback value")
                    })?;
                    call_args.push(Self::prop_get(val, "val"));
                }
                if pass_receiver {
                    let r = recv.ok_or_else(|| {
                        Diagnostic::internal("missing receiver for callback call")
                    })?;
                    call_args.push(r);
                }
                let arity = match view.kind {
                    TemplateKind::PromiseResolve(_) | TemplateKind::PromiseReject(_) => {
                        self.types.tuple_slice(target_args).len()
                    }
                    _ if view.params.iter().any(|p| p.is_rest) => args.len(),
                    _ => view.params.len(),
                };
                call_args.extend(args.into_iter().take(arity));
                let ret_ty = self.solved.locations[ret_loc];
                let returns_promise = view.is_async || self.types.is_promise_ty(ret_ty);
                Ok((Self::synth_call(fid, call_args, 0, 0), returns_promise))
            }
            CallbackCall::Dispatch { helper, needs_recv } => {
                let fid = self.emit_helper(helper)?;
                let val = cb_val
                    .ok_or_else(|| Diagnostic::internal("missing dispatcher callback value"))?;
                let mut call_args = Vec::with_capacity(args.len() + 2);
                call_args.push(val);
                if needs_recv {
                    let r = recv.ok_or_else(|| {
                        Diagnostic::internal("missing receiver for dispatcher call")
                    })?;
                    call_args.push(r);
                }
                call_args.extend(args);
                let returns_promise = self
                    .synth_functions
                    .iter()
                    .find(|f| f.id == fid)
                    .is_some_and(|f| f.is_async || matches!(f.return_type, HirType::Promise(_)));
                Ok((Self::synth_call(fid, call_args, 0, 0), returns_promise))
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_promise_new(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        value_loc: LocId,
        resolve_tid: TemplateId,
        reject_tid: TemplateId,
        executor: CallbackCall,
        pass_resolve: bool,
        pass_reject: bool,
    ) -> Result<Expr> {
        let inner_hir = self.promise_inner_hir(value_loc)?;
        let prom_hir = HirType::Promise(Box::new(inner_hir.clone()));
        let owner = CaptureOwner::Instance(rctx.inst_id);
        let p_id = self.fresh_synthetic_local()?;
        let mut body = Vec::with_capacity(6);
        let arg_ctx = rctx.with_expected(true);
        let mut evaluated = Vec::new();
        let effects = self.project_arguments(&arg_ctx, args, &mut evaluated, |_, i| {
            i == 0 && executor.needs_value()
        })?;
        if let CallbackCall::ThrowTypeError(helper) = executor {
            return Ok(sequence(
                effects,
                Self::synth_call(self.emit_helper(helper)?, Vec::new(), 0, 0),
            ));
        }
        let binding = evaluated
            .pop()
            .map(|value| self.fresh_synthetic_local().map(|id| (id, value)))
            .transpose()?;
        let exec_val = binding.as_ref().map(|(id, _)| Expr::LocalGet(*id));
        if let (Some((id, _)), CallbackCall::Dispatch { helper, .. }) = (&binding, executor)
            && let HelperSpec::Dispatcher { callee_ty, .. } = self.solved.helpers[helper]
            && self.nullish_flags(callee_ty) != (false, false)
        {
            body.push(Stmt::If {
                condition: Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(Expr::LocalGet(*id)),
                },
                then_branch: vec![Stmt::Throw(Expr::TypeErrorNew(Box::new(Expr::String(
                    "Promise executor is not a function".into(),
                ))))],
                else_branch: None,
            });
        }
        body.push(Stmt::Let {
            id: p_id,
            name: "__p".to_owned(),
            ty: prom_hir.clone(),
            mutable: false,
            init: Some(GuestCall::Pending(inner_hir).into_hir()),
        });
        let mut exec_args = Vec::with_capacity(2);
        for template in [
            pass_resolve.then_some(resolve_tid),
            pass_reject.then_some(reject_tid),
        ]
        .into_iter()
        .flatten()
        {
            let ident = FuncIdentity {
                template,
                capture_owner: owner,
            };
            let env_cls = self.ensure_env_class(ident)?;
            let env_new = Expr::New {
                class_name: env_cls,
                args: vec![Expr::LocalGet(p_id)],
                type_args: Vec::new(),
                byte_offset: 0,
                byte_end: 0,
                cap_args_appended: 0,
            };
            exec_args.push(Self::emit_env_wrapper(ident, env_new));
        }
        let (exec_call, _) = self.emit_callback_call_expr(executor, exec_val, None, exec_args)?;
        let err_id = self.fresh_synthetic_local()?;
        body.push(Stmt::Try {
            body: vec![Stmt::Expr(exec_call)],
            catch: Some(CatchClause {
                param: Some((err_id, "__e".to_owned())),
                body: vec![Stmt::Expr(
                    GuestCall::Reject {
                        promise: Expr::LocalGet(p_id),
                        reason: Expr::LocalGet(err_id),
                    }
                    .into_hir(),
                )],
            }),
            finally: None,
            forof_completion: None,
        });
        body.push(Stmt::Return(Some(Expr::LocalGet(p_id))));
        let call = self.inline_iife(prom_hir, body)?;
        let call = match binding {
            Some((id, value)) => Expr::ScopedTemp {
                id,
                value: Box::new(value),
                body: Box::new(call),
            },
            None => call,
        };
        Ok(sequence(effects, call))
    }

    fn return_cb_stmts(call: Expr, returns_promise: bool, is_void_ret: bool) -> Vec<Stmt> {
        if is_void_ret {
            let stmt = if returns_promise {
                Stmt::Expr(Expr::Await(Box::new(call)))
            } else {
                Stmt::Expr(call)
            };
            vec![stmt, Stmt::Return(None)]
        } else {
            vec![Stmt::Return(Some(call))]
        }
    }

    fn admits_undef_hir(ty: &HirType) -> bool {
        match ty {
            HirType::Void => true,
            HirType::Union(ms) => ms.contains(&HirType::Void),
            _ => false,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn build_promise_reaction(
        &mut self,
        fid: FuncId,
        kind: ReactionKind,
        promise_ty: Ty,
        inner_ty: Ty,
        on_fulfilled: Option<(CallbackCall, Ty)>,
        on_rejected: Option<(CallbackCall, Ty)>,
        ret_ty: Ty,
    ) -> Result<Function> {
        let p_pid = self.fresh_synthetic_local()?;
        let prom_hir = self.materialize(promise_ty)?;
        let mut params = vec![Self::synth_param(p_pid, "__p".to_owned(), prom_hir)];
        let mut handler_param =
            |handler: Option<(CallbackCall, Ty)>, name: &str| -> Result<Option<LocalId>> {
                let Some((_, ty)) = handler.filter(|(cb, _)| cb.needs_value()) else {
                    return Ok(None);
                };
                let id = self.fresh_synthetic_local()?;
                params.push(Self::synth_param(id, name.into(), self.materialize(ty)?));
                Ok(Some(id))
            };
        let f_pid = handler_param(on_fulfilled, "__f")?;
        let g_pid = handler_param(on_rejected, "__g")?;
        let optional = |handler: Option<(CallbackCall, Ty)>, id: Option<LocalId>| {
            id.filter(|_| handler.is_some_and(|(_, ty)| self.nullish_flags(ty) != (false, false)))
        };
        let optional_f = optional(on_fulfilled, f_pid);
        let optional_g = optional(on_rejected, g_pid);
        let when_callable =
            |id: Option<LocalId>, body: Vec<Stmt>, otherwise: Option<Vec<Stmt>>| match id {
                Some(id) => vec![Stmt::If {
                    condition: Expr::LocalGet(id),
                    then_branch: body,
                    else_branch: otherwise,
                }],
                None => body,
            };
        let ret_hir = if ret_ty == Ty::BOTTOM {
            HirType::Void
        } else {
            self.materialize(ret_ty)?
        };
        let inner_hir = if inner_ty == Ty::BOTTOM {
            HirType::Void
        } else {
            self.materialize(inner_ty)?
        };
        let is_void_ret = ret_hir == HirType::Void;
        let is_void_inner = inner_hir == HirType::Void;
        let await_p = || Expr::Await(Box::new(Expr::LocalGet(p_pid)));

        let body = match (kind, on_fulfilled, on_rejected) {
            (ReactionKind::Finally, Some((f_call, _)), _) => {
                let (call, asynchronous) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    Vec::new(),
                )?;
                let body = if is_void_inner {
                    vec![Stmt::Expr(await_p())]
                } else {
                    vec![Stmt::Return(Some(await_p()))]
                };
                vec![Stmt::Try {
                    body,
                    catch: None,
                    finally: Some(when_callable(
                        optional_f,
                        vec![Stmt::Expr(if asynchronous {
                            Expr::Await(Box::new(call))
                        } else {
                            call
                        })],
                        None,
                    )),
                    forof_completion: None,
                }]
            }
            (ReactionKind::Then, Some((f_call, _)), Some((g_call, _))) => {
                let catch_id = self.fresh_synthetic_local()?;
                let mut stmts = Vec::with_capacity(4);
                let val_slot = if !is_void_inner {
                    let vid = self.fresh_synthetic_local()?;
                    stmts.push(Stmt::Let {
                        id: vid,
                        name: "__v".to_owned(),
                        ty: Self::admitting_undefined_hir(inner_hir.clone()),
                        mutable: true,
                        init: Some(Expr::Undefined),
                    });
                    Some(vid)
                } else {
                    None
                };
                let try_stmt = match val_slot {
                    Some(vid) => Stmt::Expr(Expr::LocalSet(vid, Box::new(await_p()))),
                    None => Stmt::Expr(await_p()),
                };
                let (g_expr, g_is_prom) = self.emit_callback_call_expr(
                    g_call,
                    g_pid.map(Expr::LocalGet),
                    None,
                    vec![Expr::LocalGet(catch_id)],
                )?;
                stmts.push(Stmt::Try {
                    body: vec![try_stmt],
                    catch: Some(CatchClause {
                        param: Some((catch_id, "__e".to_owned())),
                        body: when_callable(
                            optional_g,
                            Self::return_cb_stmts(g_expr, g_is_prom, is_void_ret),
                            optional_g.map(|_| vec![Stmt::Throw(Expr::LocalGet(catch_id))]),
                        ),
                    }),
                    finally: None,
                    forof_completion: None,
                });
                let f_arg = match val_slot {
                    Some(vid) => {
                        let v = Expr::LocalGet(vid);
                        if Self::admits_undef_hir(&inner_hir) {
                            v
                        } else {
                            Self::non_null_unwrap(v)
                        }
                    }
                    None => Expr::Undefined,
                };
                let passthrough =
                    optional_f.map(|_| vec![Stmt::Return((!is_void_ret).then(|| f_arg.clone()))]);
                let (f_expr, f_is_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    vec![f_arg],
                )?;
                stmts.extend(when_callable(
                    optional_f,
                    Self::return_cb_stmts(f_expr, f_is_prom, is_void_ret),
                    passthrough,
                ));
                stmts
            }
            (ReactionKind::Then, Some((f_call, _)), None) => {
                let mut stmts = Vec::with_capacity(3);
                let f_arg = if is_void_inner {
                    stmts.push(Stmt::Expr(await_p()));
                    Expr::Undefined
                } else {
                    let vid = self.fresh_synthetic_local()?;
                    stmts.push(Stmt::Let {
                        id: vid,
                        name: "__v".to_owned(),
                        ty: inner_hir,
                        mutable: false,
                        init: Some(await_p()),
                    });
                    Expr::LocalGet(vid)
                };
                let passthrough =
                    optional_f.map(|_| vec![Stmt::Return((!is_void_ret).then(|| f_arg.clone()))]);
                let (f_expr, f_is_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    vec![f_arg],
                )?;
                stmts.extend(when_callable(
                    optional_f,
                    Self::return_cb_stmts(f_expr, f_is_prom, is_void_ret),
                    passthrough,
                ));
                stmts
            }
            (_, None, Some((g_call, _))) => {
                let catch_id = self.fresh_synthetic_local()?;
                let try_body = if is_void_ret {
                    vec![Stmt::Expr(await_p()), Stmt::Return(None)]
                } else if matches!(inner_ty, Ty::BOTTOM | Ty::NEVER) {
                    vec![
                        Stmt::Expr(await_p()),
                        Stmt::Throw(Expr::ErrorNew(Some(Box::new(Expr::String(
                            "unreachable".to_owned(),
                        ))))),
                    ]
                } else {
                    vec![Stmt::Return(Some(await_p()))]
                };
                let (g_expr, g_is_prom) = self.emit_callback_call_expr(
                    g_call,
                    g_pid.map(Expr::LocalGet),
                    None,
                    vec![Expr::LocalGet(catch_id)],
                )?;
                vec![Stmt::Try {
                    body: try_body,
                    catch: Some(CatchClause {
                        param: Some((catch_id, "__e".to_owned())),
                        body: when_callable(
                            optional_g,
                            Self::return_cb_stmts(g_expr, g_is_prom, is_void_ret),
                            optional_g.map(|_| vec![Stmt::Throw(Expr::LocalGet(catch_id))]),
                        ),
                    }),
                    finally: None,
                    forof_completion: None,
                }]
            }
            _ => {
                if is_void_ret {
                    vec![Stmt::Expr(await_p())]
                } else {
                    vec![Stmt::Return(Some(await_p()))]
                }
            }
        };

        let prefix = match kind {
            ReactionKind::Then => "__then",
            ReactionKind::Catch => "__catch",
            ReactionKind::Finally => "__finally",
        };
        Ok(Self::synth_function(
            fid,
            format!("{prefix}_{fid}"),
            params,
            HirType::Promise(Box::new(ret_hir)),
            body,
            true,
        ))
    }

    pub(super) fn build_timer_runner(
        &mut self,
        fid: FuncId,
        repeating: bool,
        is_immediate: bool,
        cb_call: CallbackCall,
        cb_ty: Ty,
        extra_arg_tys: Ty,
    ) -> Result<Function> {
        let timer_cls = self.ensure_timer_class()?;
        let timer_pid = self.fresh_synthetic_local()?;
        let mut params = vec![Self::synth_param(
            timer_pid,
            "__timer".to_owned(),
            HirType::Named(timer_cls),
        )];
        let cb_pid = if cb_call.needs_value() {
            let id = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(
                id,
                "__cb".to_owned(),
                self.materialize(cb_ty)?,
            ));
            Some(id)
        } else {
            None
        };
        let delay_pid = if !is_immediate {
            let id = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(id, "__delay".to_owned(), HirType::Number));
            Some(id)
        } else {
            None
        };
        let extra_slice = self.types.tuple_slice(extra_arg_tys);
        let mut extra_pids = Vec::with_capacity(extra_slice.len());
        for (i, &aty) in extra_slice.iter().enumerate() {
            let id = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(
                id,
                format!("__a{i}"),
                self.materialize(aty)?,
            ));
            extra_pids.push(id);
        }
        let delay_expr = match delay_pid {
            Some(id) => Expr::LocalGet(id),
            None => Expr::Number(0.0),
        };
        let signal_expr =
            Self::prop_get(Self::prop_get(Expr::LocalGet(timer_pid), "ctrl"), "signal");
        let wait_stmt = Stmt::Try {
            body: vec![Stmt::Expr(Expr::Await(Box::new(
                GuestCall::Timer {
                    delay: delay_expr,
                    signal: signal_expr,
                }
                .into_hir(),
            )))],
            catch: Some(CatchClause {
                param: None,
                body: vec![Stmt::Return(None)],
            }),
            finally: None,
            forof_completion: None,
        };
        let cancel_check = Stmt::If {
            condition: Self::prop_get(Expr::LocalGet(timer_pid), "cancelled"),
            then_branch: vec![Stmt::Return(None)],
            else_branch: None,
        };
        let (cb_expr, _) = self.emit_callback_call_expr(
            cb_call,
            cb_pid.map(Expr::LocalGet),
            Some(Expr::LocalGet(timer_pid)),
            extra_pids.into_iter().map(Expr::LocalGet).collect(),
        )?;
        let step = vec![wait_stmt, cancel_check, Stmt::Expr(cb_expr)];
        let body = if repeating {
            vec![Stmt::While {
                condition: Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(Self::prop_get(Expr::LocalGet(timer_pid), "cancelled")),
                },
                body: step,
            }]
        } else {
            step
        };
        Ok(Self::synth_function(
            fid,
            format!("__timer_{fid}"),
            params,
            HirType::Promise(Box::new(HirType::Void)),
            body,
            true,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_timer_start(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        helper: HelperId,
        has_delay: bool,
        is_immediate: bool,
        pass_cb: bool,
        byte_offset: u32,
        byte_end: u32,
    ) -> Result<Expr> {
        let fid = self.emit_helper(helper)?;
        let timer_cls = self.ensure_timer_class()?;
        let t_id = self.fresh_synthetic_local()?;
        let arg_ctx = rctx.with_expected(true);
        let mut body = Vec::with_capacity(4);
        if !pass_cb
            && let Some(cb_expr) = args.first()
            && !is_pure_func_expr(cb_expr)
        {
            body.push(Stmt::Expr(self.rewrite_expr(rctx, cb_expr)?));
        }
        body.push(Stmt::Let {
            id: t_id,
            name: "__t".to_owned(),
            ty: HirType::Named(timer_cls.clone()),
            mutable: false,
            init: Some(Expr::New {
                class_name: timer_cls.clone(),
                args: vec![
                    Expr::New {
                        class_name: "AbortController".to_owned(),
                        args: Vec::new(),
                        type_args: Vec::new(),
                        byte_offset: 0,
                        byte_end: 0,
                        cap_args_appended: 0,
                    },
                    Expr::Bool(false),
                ],
                type_args: Vec::new(),
                byte_offset: 0,
                byte_end: 0,
                cap_args_appended: 0,
            }),
        });
        let mut call_args = vec![Expr::LocalGet(t_id)];
        if pass_cb && let Some(cb_expr) = args.first() {
            call_args.push(self.rewrite_expr(&arg_ctx, cb_expr)?);
        }
        if !is_immediate {
            let delay_expr = if has_delay && let Some(d) = args.get(1) {
                self.rewrite_expr(&arg_ctx, d)?
            } else {
                Expr::Number(0.0)
            };
            call_args.push(delay_expr);
        }
        let extra_start = if is_immediate { 1 } else { 2 };
        for extra in args.get(extra_start..).unwrap_or(&[]) {
            call_args.push(self.rewrite_expr(&arg_ctx, extra)?);
        }
        body.push(Stmt::Expr(Self::synth_call(
            fid,
            call_args,
            byte_offset,
            byte_end,
        )));
        body.push(Stmt::Return(Some(Expr::LocalGet(t_id))));
        self.inline_iife(HirType::Named(timer_cls), body)
    }

    pub(super) fn emit_timer_clear(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        arg_ty: Option<Ty>,
    ) -> Result<Expr> {
        let mut values = Vec::new();
        let effects = self.project_arguments(rctx, args, &mut values, |_, i| {
            i == 0 && arg_ty.is_some_and(|t| !matches!(t, Ty::VOID | Ty::NULL))
        })?;
        let (Some(rewritten), Some(ty)) = (values.pop(), arg_ty) else {
            return Ok(sequence(effects, Expr::Undefined));
        };
        let timer_cls = self.ensure_timer_class()?;
        let (has_null, has_undef) = self.nullish_flags(ty);
        let h_ty = if self.is_resolved(ty) {
            self.materialize(ty)?
        } else {
            HirType::Named(timer_cls)
        };
        let h_id = self.fresh_synthetic_local()?;
        let target = if has_null || has_undef {
            Self::non_null_unwrap(Expr::LocalGet(h_id))
        } else {
            Expr::LocalGet(h_id)
        };
        let cancel = vec![
            Stmt::Expr(Expr::PropertySet {
                object: Box::new(target.clone()),
                property: "cancelled".to_owned(),
                value: Box::new(Expr::Bool(true)),
                byte_offset: 0,
                byte_end: 0,
            }),
            Stmt::Expr(Self::method_call_expr(
                Self::prop_get(target, "ctrl"),
                "abort",
                Vec::new(),
            )),
        ];
        let mut body = vec![Stmt::Let {
            id: h_id,
            name: "__h".to_owned(),
            ty: h_ty,
            mutable: false,
            init: Some(rewritten),
        }];
        if has_null || has_undef {
            body.push(Stmt::If {
                condition: Expr::LocalGet(h_id),
                then_branch: cancel,
                else_branch: None,
            });
        } else {
            body.extend(cancel);
        }
        self.inline_iife(HirType::Void, body)
    }
}
