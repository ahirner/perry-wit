//! Emission of guest-settled promises (`new Promise`, `then`/`catch`/`finally`) and global timers.
use super::{Emitter, RewriteCtx, is_pure_func_expr};
use crate::engine::boundary::{TemplateKind, TemplateView};
use crate::engine::solve::{CallbackCall, Instance, ReactionKind};
use crate::engine::types::{CaptureOwner, FuncIdentity, HelperId, LocId, TemplateId, Ty};
use perry_hir::{
    ir::{
        CatchClause, Class, ClassField, ClassFieldOrigin, CompareOp, Expr, Function, LogicalOp,
        Stmt, UnaryOp,
    },
    types::{FuncId, Type as HirType},
};
use perry_wit_diagnostics::{Diagnostic, Result};

const TIMER_CLASS: &str = "__PerryTimer";

impl<'h> Emitter<'h, '_> {
    fn extern_call(name: &str, args: Vec<Expr>, type_args: Vec<HirType>) -> Expr {
        Expr::Call {
            callee: Box::new(Expr::ExternFuncRef {
                name: name.to_owned(),
                param_types: Vec::new(),
                return_type: HirType::Any,
            }),
            args,
            type_args,
            byte_offset: 0,
            byte_end: 0,
        }
    }

    fn inline_iife(&mut self, return_type: HirType, body: Vec<Stmt>) -> Result<Expr> {
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
        let elem = self
            .solved
            .locations
            .get(value_loc)
            .copied()
            .unwrap_or(Ty::BOTTOM);
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
        let value_loc = *self
            .solved
            .promise_handle_locs
            .get(&(ident.template, ident.capture_owner))
            .ok_or_else(|| Diagnostic::internal("missing promise handle value location"))?;
        let prom_ty = HirType::Promise(Box::new(self.promise_inner_hir(value_loc)?));
        let pid = self.fresh_synthetic_local()?;
        let ctor_fid = self.fresh_func_id()?;
        let ctor = Self::synth_function(
            ctor_fid,
            "constructor".to_owned(),
            vec![Self::synth_param(pid, "c0".to_owned(), prom_ty.clone())],
            HirType::Void,
            vec![Stmt::Expr(Expr::PropertySet {
                object: Box::new(Expr::This),
                property: "c0".to_owned(),
                value: Box::new(Expr::LocalGet(pid)),
                byte_offset: 0,
                byte_end: 0,
            })],
            false,
        );
        let id = self.next_class_id;
        self.next_class_id = id.saturating_add(1);
        self.synth_classes.push(Class {
            id,
            name: name.clone(),
            type_params: Vec::new(),
            extends: None,
            extends_name: None,
            native_extends: None,
            extends_expr: None,
            heritage_lexically_shadowed: false,
            fields: vec![ClassField {
                origin: ClassFieldOrigin::Declared,
                name: "c0".to_owned(),
                key_expr: None,
                ty: prom_ty,
                init: None,
                is_private: false,
                is_readonly: false,
                decorators: Vec::new(),
            }],
            constructor: Some(ctor),
            methods: Vec::new(),
            getters: Vec::new(),
            setters: Vec::new(),
            static_accessor_names: Vec::new(),
            static_accessor_fn_ids: Vec::new(),
            static_fields: Vec::new(),
            static_methods: Vec::new(),
            computed_members: Vec::new(),
            decorators: Vec::new(),
            is_exported: false,
            aliases: Vec::new(),
            is_nested: false,
            alloc_width_hint: 0,
            specialized_from: None,
            decl_stmt: None,
        });
        Ok(name)
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
        let marker = if view.kind == TemplateKind::PromiseResolve {
            "\0perry_promise_resolve"
        } else {
            "\0perry_promise_reject"
        };
        let body = vec![Stmt::Expr(Self::extern_call(
            marker,
            vec![Self::prop_get(Expr::LocalGet(env_pid), "c0"), val_expr],
            Vec::new(),
        ))];
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
        let ctrl_pid = self.fresh_synthetic_local()?;
        let cancelled_pid = self.fresh_synthetic_local()?;
        let ctrl_ty = HirType::Named("AbortController".to_owned());
        let ctor_fid = self.fresh_func_id()?;
        let ctor = Self::synth_function(
            ctor_fid,
            "constructor".to_owned(),
            vec![
                Self::synth_param(ctrl_pid, "ctrl".to_owned(), ctrl_ty.clone()),
                Self::synth_param(cancelled_pid, "cancelled".to_owned(), HirType::Boolean),
            ],
            HirType::Void,
            vec![
                Stmt::Expr(Expr::PropertySet {
                    object: Box::new(Expr::This),
                    property: "ctrl".to_owned(),
                    value: Box::new(Expr::LocalGet(ctrl_pid)),
                    byte_offset: 0,
                    byte_end: 0,
                }),
                Stmt::Expr(Expr::PropertySet {
                    object: Box::new(Expr::This),
                    property: "cancelled".to_owned(),
                    value: Box::new(Expr::LocalGet(cancelled_pid)),
                    byte_offset: 0,
                    byte_end: 0,
                }),
            ],
            false,
        );
        let id = self.next_class_id;
        self.next_class_id = id.saturating_add(1);
        self.synth_classes.push(Class {
            id,
            name: TIMER_CLASS.to_owned(),
            type_params: Vec::new(),
            extends: None,
            extends_name: None,
            native_extends: None,
            extends_expr: None,
            heritage_lexically_shadowed: false,
            fields: vec![
                ClassField {
                    origin: ClassFieldOrigin::Declared,
                    name: "ctrl".to_owned(),
                    key_expr: None,
                    ty: ctrl_ty,
                    init: None,
                    is_private: false,
                    is_readonly: false,
                    decorators: Vec::new(),
                },
                ClassField {
                    origin: ClassFieldOrigin::Declared,
                    name: "cancelled".to_owned(),
                    key_expr: None,
                    ty: HirType::Boolean,
                    init: None,
                    is_private: false,
                    is_readonly: false,
                    decorators: Vec::new(),
                },
            ],
            constructor: Some(ctor),
            methods: Vec::new(),
            getters: Vec::new(),
            setters: Vec::new(),
            static_accessor_names: Vec::new(),
            static_accessor_fn_ids: Vec::new(),
            static_fields: Vec::new(),
            static_methods: Vec::new(),
            computed_members: Vec::new(),
            decorators: Vec::new(),
            is_exported: false,
            aliases: Vec::new(),
            is_nested: false,
            alloc_width_hint: 0,
            specialized_from: None,
            decl_stmt: None,
        });
        Ok(TIMER_CLASS.to_owned())
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
                    TemplateKind::PromiseResolve | TemplateKind::PromiseReject => {
                        self.types.tuple_slice(target_args).len()
                    }
                    _ if view.params.iter().any(|p| p.is_rest) => args.len(),
                    _ => view.params.len(),
                };
                call_args.extend(args.into_iter().take(arity));
                let ret_ty = self
                    .solved
                    .locations
                    .get(ret_loc)
                    .copied()
                    .unwrap_or(Ty::VOID);
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
        exec_expr: &'h Expr,
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
        let exec_val = if executor.needs_value() {
            Some(self.rewrite_expr(&arg_ctx, exec_expr)?)
        } else {
            if !is_pure_func_expr(exec_expr) {
                body.push(Stmt::Expr(self.rewrite_expr(rctx, exec_expr)?));
            }
            None
        };
        body.push(Stmt::Let {
            id: p_id,
            name: "__p".to_owned(),
            ty: prom_hir.clone(),
            mutable: false,
            init: Some(Self::extern_call(
                "\0perry_promise_pending",
                Vec::new(),
                vec![inner_hir],
            )),
        });
        let mut exec_args = Vec::with_capacity(2);
        if pass_resolve {
            let ident = FuncIdentity {
                template: resolve_tid,
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
            exec_args.push(self.emit_env_wrapper(ident, env_new));
        }
        if pass_reject {
            let ident = FuncIdentity {
                template: reject_tid,
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
            exec_args.push(self.emit_env_wrapper(ident, env_new));
        }
        let (exec_call, _) = self.emit_callback_call_expr(executor, exec_val, None, exec_args)?;
        let err_id = self.fresh_synthetic_local()?;
        body.push(Stmt::Try {
            body: vec![Stmt::Expr(exec_call)],
            catch: Some(CatchClause {
                param: Some((err_id, "__e".to_owned())),
                body: vec![Stmt::Expr(Self::extern_call(
                    "\0perry_promise_reject",
                    vec![Expr::LocalGet(p_id), Expr::LocalGet(err_id)],
                    Vec::new(),
                ))],
            }),
            finally: None,
            forof_completion: None,
        });
        body.push(Stmt::Return(Some(Expr::LocalGet(p_id))));
        self.inline_iife(prom_hir, body)
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
        let f_pid = match on_fulfilled {
            Some((cb, fty)) if cb.needs_value() => {
                let id = self.fresh_synthetic_local()?;
                params.push(Self::synth_param(
                    id,
                    "__f".to_owned(),
                    self.materialize(fty)?,
                ));
                Some(id)
            }
            _ => None,
        };
        let g_pid = match on_rejected {
            Some((cb, gty)) if cb.needs_value() => {
                let id = self.fresh_synthetic_local()?;
                params.push(Self::synth_param(
                    id,
                    "__g".to_owned(),
                    self.materialize(gty)?,
                ));
                Some(id)
            }
            _ => None,
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
                let catch_id = self.fresh_synthetic_local()?;
                let mut stmts = Vec::with_capacity(5);
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
                let (f_catch, f_catch_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    Vec::new(),
                )?;
                let (f_ok, f_ok_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    Vec::new(),
                )?;
                let try_stmt = match val_slot {
                    Some(vid) => Stmt::Expr(Expr::LocalSet(vid, Box::new(await_p()))),
                    None => Stmt::Expr(await_p()),
                };
                let run_f = |expr: Expr, is_prom: bool| {
                    if is_prom {
                        Stmt::Expr(Expr::Await(Box::new(expr)))
                    } else {
                        Stmt::Expr(expr)
                    }
                };
                stmts.push(Stmt::Try {
                    body: vec![try_stmt],
                    catch: Some(CatchClause {
                        param: Some((catch_id, "__e".to_owned())),
                        body: vec![
                            run_f(f_catch, f_catch_prom),
                            Stmt::Throw(Expr::LocalGet(catch_id)),
                        ],
                    }),
                    finally: None,
                    forof_completion: None,
                });
                stmts.push(run_f(f_ok, f_ok_prom));
                if let Some(vid) = val_slot
                    && !is_void_ret
                {
                    let v = Expr::LocalGet(vid);
                    let v = if Self::admits_undef_hir(&inner_hir) {
                        v
                    } else {
                        Self::non_null_unwrap(v)
                    };
                    stmts.push(Stmt::Return(Some(v)));
                }
                stmts
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
                        body: Self::return_cb_stmts(g_expr, g_is_prom, is_void_ret),
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
                let (f_expr, f_is_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    vec![f_arg],
                )?;
                stmts.extend(Self::return_cb_stmts(f_expr, f_is_prom, is_void_ret));
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
                let (f_expr, f_is_prom) = self.emit_callback_call_expr(
                    f_call,
                    f_pid.map(Expr::LocalGet),
                    None,
                    vec![f_arg],
                )?;
                stmts.extend(Self::return_cb_stmts(f_expr, f_is_prom, is_void_ret));
                stmts
            }
            (_, None, Some((g_call, _))) => {
                let catch_id = self.fresh_synthetic_local()?;
                let try_body = if is_void_ret {
                    vec![Stmt::Expr(await_p()), Stmt::Return(None)]
                } else if inner_ty == Ty::BOTTOM {
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
                        body: Self::return_cb_stmts(g_expr, g_is_prom, is_void_ret),
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
        let delay_pid = if !is_immediate {
            let id = self.fresh_synthetic_local()?;
            params.push(Self::synth_param(id, "__delay".to_owned(), HirType::Number));
            Some(id)
        } else {
            None
        };
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
        let extra_slice: Vec<Ty> = self.types.tuple_slice(extra_arg_tys).to_vec();
        let mut extra_pids = Vec::with_capacity(extra_slice.len());
        for (i, aty) in extra_slice.into_iter().enumerate() {
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
            body: vec![Stmt::Expr(Expr::Await(Box::new(Self::extern_call(
                "\0perry_timer",
                vec![delay_expr, signal_expr],
                Vec::new(),
            ))))],
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
        if !is_immediate {
            let delay_expr = if has_delay && let Some(d) = args.get(1) {
                self.rewrite_expr(&arg_ctx, d)?
            } else {
                Expr::Number(0.0)
            };
            call_args.push(delay_expr);
        }
        if pass_cb && let Some(cb_expr) = args.first() {
            call_args.push(self.rewrite_expr(&arg_ctx, cb_expr)?);
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
        let Some(handle_expr) = args.first() else {
            return Ok(Expr::Undefined);
        };
        let Some(ty) = arg_ty else {
            return Ok(Expr::Undefined);
        };
        if matches!(ty, Ty::VOID | Ty::NULL) {
            return Ok(if is_pure_func_expr(handle_expr) {
                Expr::Undefined
            } else {
                Expr::Sequence(vec![self.rewrite_expr(rctx, handle_expr)?, Expr::Undefined])
            });
        }
        let timer_cls = self.ensure_timer_class()?;
        let (has_null, has_undef) = self.nullish_flags(ty);
        let h_ty = if self.is_resolved(ty) {
            self.materialize(ty)?
        } else {
            HirType::Named(timer_cls)
        };
        let h_id = self.fresh_synthetic_local()?;
        let rewritten = self.rewrite_expr(rctx, handle_expr)?;
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
        let cond = match (has_null, has_undef) {
            (true, true) => Some(Expr::Logical {
                op: LogicalOp::And,
                left: Box::new(Expr::Compare {
                    op: CompareOp::Ne,
                    left: Box::new(Expr::LocalGet(h_id)),
                    right: Box::new(Expr::Null),
                }),
                right: Box::new(Expr::Compare {
                    op: CompareOp::Ne,
                    left: Box::new(Expr::LocalGet(h_id)),
                    right: Box::new(Expr::Undefined),
                }),
            }),
            (true, false) => Some(Expr::Compare {
                op: CompareOp::Ne,
                left: Box::new(Expr::LocalGet(h_id)),
                right: Box::new(Expr::Null),
            }),
            (false, true) => Some(Expr::Compare {
                op: CompareOp::Ne,
                left: Box::new(Expr::LocalGet(h_id)),
                right: Box::new(Expr::Undefined),
            }),
            (false, false) => None,
        };
        match cond {
            Some(condition) => body.push(Stmt::If {
                condition,
                then_branch: cancel,
                else_branch: None,
            }),
            None => body.extend(cancel),
        }
        self.inline_iife(HirType::Void, body)
    }
}
