//! Call, method and constructor resolution.
use super::*;
use crate::engine::types::WitView;

#[derive(Clone, Copy)]
pub(super) enum CallOrigin<'h> {
    Source(SourceCall<'h>),
    Callback(u32, &'h [HirType]),
}

impl CallOrigin<'_> {
    pub(super) fn offset(self) -> u32 {
        match self {
            Self::Source(call) => call.offset,
            Self::Callback(offset, _) => offset,
        }
    }
}

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn eval_args(&mut self, s: &mut EvalState<'h>, args: &'h [Expr]) -> Result<Vec<Ty>> {
        args.iter().map(|a| self.eval_expr(s, a)).collect()
    }

    pub(super) fn eval_type_args(&mut self, inst_id: InstanceId, type_args: &[HirType]) -> Vec<Ty> {
        if type_args.is_empty() {
            return Vec::new();
        }
        let bindings = &self.type_bindings[inst_id];
        type_args
            .iter()
            .map(|ta| {
                let t = self.types.intern_hir(self.ctx, ta, true);
                self.index.substitute(self.types, t, bindings)
            })
            .collect()
    }

    pub(super) fn eval_object_fields<'b>(
        &mut self,
        s: &mut EvalState<'h>,
        pairs: impl Iterator<Item = (&'b str, &'h Expr)>,
    ) -> Result<Ty> {
        let mut props = Vec::new();
        for (name, expr) in pairs {
            let ty = self.eval_expr(s, expr)?;
            self.invalidate_conservative_cell(ty);
            self.check_escaping_func(s.inst, ty, 0, s.current_span)?;
            self.site_mut(s.inst, expr)?.erased = self
                .types
                .is_zero_sized_func(ty, &self.index.capture_free_templates);
            let ty = self.types.normalize_for_storage(self.ctx, self.decls, ty);
            props.push(self.types.property(name, ty, false));
        }
        if props.iter().any(|p| p.ty == Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        Ok(self.types.object(&props, None))
    }

    pub(super) fn resource_result(&mut self, member: crate::wit::MemberId) -> Ty {
        member
            .signature(self.decls.world)
            .result
            .and_then(|value| {
                Some(crate::wit::WitValue {
                    ty: value.result().map_or(Some(value.ty), |r| r.ok)?,
                    ..value
                })
            })
            .map_or(Ty::VOID, |value| {
                self.types.wit_type(self.ctx, value, WitView::Reference)
            })
    }

    pub(super) fn class_call(
        &mut self,
        inst_id: InstanceId,
        call: SourceCall<'h>,
        template: TemplateId,
        receiver: Option<ClassInstId>,
        inputs: &[Ty],
    ) -> Result<Ty> {
        if inputs.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        let view = self.index.view(template);
        self.check_callback_params(inst_id, view, inputs, call.offset)?;
        let explicit = self.eval_type_args(inst_id, call.type_args);
        let target = self.request_instance(
            InstanceReq::new(template, view.accepted_inputs(inputs), Some(inst_id))
                .receiver(receiver.map(Receiver::Class))
                .type_args(&explicit)
                .at(call.offset),
        )?;
        let call_target = FunctionTarget::Direct(target).call(receiver.is_some());
        self.record_site(inst_id, call.site, SiteResolution::Call(call_target));
        let return_loc = self.instances[target].return_loc;
        Ok(self.read_loc(return_loc, inst_id))
    }

    pub(super) fn resolve_class_inst(
        &mut self,
        inst_id: InstanceId,
        stripped: Ty,
        byte_offset: u32,
    ) -> Result<Option<ClassInstId>> {
        match self.types.kind(stripped) {
            TyKind::ClassInst(id) => Ok(Some(id)),
            TyKind::Named(id) => self
                .decls
                .symbol(id)
                .and_then(|s| s.class_id())
                .map(|cid| self.request_class_inst(cid, &[], byte_offset, Some(inst_id)))
                .transpose(),
            _ => Ok(None),
        }
    }

    pub(super) fn eval_new(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        class_name: &str,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
    ) -> Result<Ty> {
        if let Some(fields) = self.index.expressions.literal(expr) {
            return self.eval_object_fields(
                s,
                fields.iter().zip(args).map(|(f, a)| (f.name.as_str(), a)),
            );
        }
        let class = self
            .index
            .expressions
            .symbol(self.decls, expr)
            .and_then(|s| s.class_id());
        if class_name == "Promise" && class.is_none() {
            return self.eval_new_promise(s, expr, args, type_args, byte_offset);
        }
        let arg_tys = self.eval_args(s, args)?;
        if let Some(cid) = class {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let explicit = self.eval_type_args(s.inst, type_args);
            let class_inst =
                self.construct_class(cid, &arg_tys, explicit, byte_offset, Some(s.inst))?;
            for &ty in &arg_tys {
                self.mark_escaping_env(ty);
            }
            self.record_site(s.inst, expr, SiteResolution::ClassNew(class_inst));
            return Ok(self.types.class_inst(class_inst));
        }
        self.check_host_arguments(s, class_name, &arg_tys, byte_offset)?;
        Ok(if class_name == "Array" {
            self.types.array(Ty::OPEN)
        } else {
            self.types.named(self.ctx, class_name)
        })
    }

    pub(super) fn construct_class(
        &mut self,
        cid: ClassId,
        arg_tys: &[Ty],
        explicit: Vec<Ty>,
        byte_offset: u32,
        caller: Option<InstanceId>,
    ) -> Result<ClassInstId> {
        let count = self.index.class(cid).declaration.type_params.len();
        let class_args = if !explicit.is_empty() {
            explicit
        } else if count != 0 {
            let mut bindings = vec![Ty::OPEN; count];
            if let Some(ctor_tid) = self.index.class(cid).ctor {
                let ctor_view = self.index.view(ctor_tid);
                self.unify_params(
                    ctor_view.params,
                    arg_tys,
                    GenericOwner::Class(cid),
                    &mut bindings,
                    caller,
                )?;
            }
            bindings
        } else if self.index.class(cid).implicit_params {
            arg_tys.to_vec()
        } else {
            Vec::new()
        };
        self.request_class_inst(cid, &class_args, byte_offset, caller)
    }

    pub(super) fn eval_call(&mut self, s: &mut EvalState<'h>, call: SourceCall<'h>) -> Result<Ty> {
        let SourceCall {
            site: expr,
            callee,
            offset: byte_offset,
            ..
        } = call;
        let callee = match callee {
            crate::source::Callee::Value(callee) => callee,
            crate::source::Callee::Static { method, .. } => {
                let arg_tys = self.call_argument_types(s, call.args)?;
                if arg_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                if let Some(cid) = self
                    .index
                    .expressions
                    .symbol(self.decls, expr)
                    .and_then(|s| s.class_id())
                    && let Some(tid) = self.index.method_template(
                        &self.index.class(cid).declaration.static_methods,
                        hir_compat::method(method).name(),
                    )
                {
                    return self.class_call(s.inst, call, tid, None, &arg_tys);
                }
                return Ok(Ty::OPEN);
            }
        };
        use crate::shapes::Imported;
        let binding = self
            .index
            .expressions
            .symbol(self.decls, callee)
            .and_then(|s| s.import);
        let builtin = match (callee, binding) {
            (_, Some(Imported::Timer(name))) => Some(name),
            (Expr::ExternFuncRef { name, .. }, None) => Some(name.as_str()),
            _ => match hir_compat::member(callee) {
                Some((Expr::GlobalGet(_), hir_compat::MemberKey::Named(name, _))) => {
                    Some(hir_compat::method(name).name())
                }
                _ => None,
            },
        };
        if let (Some(builtin), Some(args)) = (builtin, call.args.plain()) {
            match builtin {
                "queueMicrotask" => {
                    return Err(self.annotate_expr_error(
                        s,
                        Diagnostic::unsupported(
                            "queueMicrotask is not supported: this runtime has no microtask queue",
                        ),
                        byte_offset,
                    ));
                }
                "setTimeout" | "setInterval" | "setImmediate" => {
                    return self.eval_timer_start(s, expr, builtin, args, byte_offset);
                }
                "clearTimeout" | "clearInterval" | "clearImmediate" => {
                    return self.eval_timer_clear(s, expr, args);
                }
                _ => {}
            }
        }
        if let (
            Expr::ExternFuncRef {
                name, return_type, ..
            },
            Some(args),
        ) = (callee, call.args.plain())
        {
            if name == "structuredClone" {
                return match args.first() {
                    Some(value) => self.eval_expr(s, value),
                    None => Ok(Ty::OPEN),
                };
            }
            let arg_tys = self.eval_args(s, args)?;
            match binding {
                Some(Imported::ReadFile) => {
                    let text = hir_compat::read_file_text(
                        args.get(1),
                        args.get(1).and_then(|e| self.index.expressions.literal(e)),
                    )
                    .map_err(|e| self.annotate_expr_error(s, e, byte_offset))?;
                    self.record_site(s.inst, expr, SiteResolution::ReadFile { text });
                    let inner = if text {
                        Ty::STRING
                    } else {
                        self.types.named(self.ctx, "Uint8Array")
                    };
                    return Ok(self.types.promise(inner));
                }
                Some(Imported::TimeoutPromise) => {
                    let value = arg_tys.get(1).copied().unwrap_or(Ty::VOID);
                    return Ok(if value == Ty::BOTTOM {
                        value
                    } else {
                        self.types.promise(value)
                    });
                }
                Some(Imported::Path(op, _) | Imported::Service(op)) => {
                    return Ok(builtins::signature_result(
                        self.ctx,
                        self.types,
                        op.result_kinds(),
                    ));
                }
                Some(Imported::WriteFile) => return Ok(self.types.promise(Ty::VOID)),
                Some(Imported::Foreign(id)) => {
                    let sig = id.signature(self.decls.world);
                    let ret = sig.result.map_or(Ty::VOID, |ty| {
                        self.types.wit_type(self.ctx, ty, WitView::Reference)
                    });
                    return Ok(
                        if sig.is_async && !matches!(self.types.kind(ret), TyKind::Promise(_)) {
                            self.types.promise(ret)
                        } else {
                            ret
                        },
                    );
                }
                _ => {}
            }
            if binding.is_none() && hir_compat::is_source_extern(name, byte_offset) {
                self.check_host_arguments(s, name, &arg_tys, byte_offset)?;
            }
            return Ok(self.types.intern_hir(self.ctx, return_type, true));
        }
        let (callee_ty, receiver) = match hir_compat::member(callee) {
            Some((object, hir_compat::MemberKey::Named(property, _))) => {
                return self.eval_method_call(s, call, callee, object, property);
            }
            Some((object, hir_compat::MemberKey::Computed(index, offset))) => {
                let receiver = self.eval_expr(s, object)?;
                if let Some(args) = call.args.plain()
                    && let TyKind::Named(name) = self.types.kind(receiver)
                    && let Some(owner) = self.decls.resource(self.ctx, name)
                    && let Some(member) = self
                        .index
                        .expressions
                        .receiver(object)
                        .and_then(|group| group.member(self.decls.world, owner))
                {
                    self.eval_args(s, args)?;
                    self.record_site(s.inst, expr, SiteResolution::ResourceCall(member));
                    return Ok(Ty::VOID);
                }
                (
                    self.eval_index(s, callee, receiver, index, offset)?,
                    Some(receiver),
                )
            }
            _ => (self.eval_expr(s, callee)?, None),
        };
        let arg_tys = self.call_argument_types(s, call.args)?;
        self.eval_callable(s, call, callee_ty, receiver, &arg_tys)
    }

    pub(super) fn eval_callable(
        &mut self,
        s: &mut EvalState<'h>,
        call: SourceCall<'h>,
        callee: Ty,
        receiver: Option<Ty>,
        inputs: &[Ty],
    ) -> Result<Ty> {
        let (result, _) =
            self.resolve_invocation(s, callee, receiver, inputs, CallOrigin::Source(call))?;
        Ok(result)
    }

    pub(super) fn resolve_invocation(
        &mut self,
        s: &mut EvalState<'h>,
        callee: Ty,
        receiver: Option<Ty>,
        inputs: &[Ty],
        invocation: CallOrigin<'h>,
    ) -> Result<(Ty, Option<CallTarget>)> {
        if callee == Ty::BOTTOM || inputs.contains(&Ty::BOTTOM) {
            return Ok((Ty::BOTTOM, None));
        }
        let offset = invocation.offset();
        let (target, result) = if matches!(callee, Ty::NEVER | Ty::NULL | Ty::VOID) {
            (CallTarget::ThrowTypeError, Ty::NEVER)
        } else if let Some(identities) = self.types.func_dispatch_members(callee, true) {
            let (helper, receiver, result) = self.callback_dispatcher(
                s,
                callee,
                &identities,
                inputs,
                callbacks::DispatchSite::Call(receiver, invocation),
            )?;
            (FunctionTarget::Dispatcher(helper).call(receiver), result)
        } else if let TyKind::Func(identity) = self.types.kind(callee) {
            for &input in inputs {
                self.invalidate_conservative_cell(input);
            }
            let (arm, result, _) =
                self.select_call_arm(s, identity, receiver, inputs, invocation)?;
            let function = if self.is_escaping_env(identity.template, s.inst) {
                FunctionTarget::Closure(arm.target)
            } else {
                FunctionTarget::Direct(arm.target)
            };
            (function.call(arm.receiver), result)
        } else if self.types.is_open(callee) || callee == Ty::DYNAMIC {
            return Ok((Ty::OPEN, None));
        } else {
            return Err(self.annotate_expr_error(
                s,
                diag!(codes::TYPE_MISMATCH, "expression is not callable"),
                offset,
            ));
        };
        if let CallOrigin::Source(call) = invocation {
            self.record_site(s.inst, call.site, SiteResolution::Call(target));
        }
        Ok((result, Some(target)))
    }

    pub(super) fn call_argument_types(
        &mut self,
        s: &mut EvalState<'h>,
        args: Arguments<'h>,
    ) -> Result<Vec<Ty>> {
        if let Some(args) = args.plain() {
            return self.eval_args(s, args);
        }
        let mut expanded = Vec::new();
        for (expr, spread) in args {
            let ty = self.eval_expr(s, expr)?;
            if spread && ty != Ty::BOTTOM {
                let (ty, _) = self.types.strip_nullish(ty);
                let ty = self.resolve_cell_read(s.inst, ty);
                match self.types.kind(ty) {
                    TyKind::Tuple(span) => expanded.extend_from_slice(self.types.ty_slice(span)),
                    _ => expanded.push(
                        builtins::iterated_element(self.ctx, self.types, ty).unwrap_or(Ty::OPEN),
                    ),
                }
            } else {
                expanded.push(ty);
            }
        }
        Ok(expanded)
    }

    pub(super) fn check_host_arguments(
        &self,
        s: &EvalState<'h>,
        name: &str,
        inputs: &[Ty],
        offset: u32,
    ) -> Result<()> {
        if inputs.iter().any(|&ty| self.types.has_func(ty)) {
            return Err(self.annotate_expr_error(s, Diagnostic::unsupported(format!(
                "passing a callback to runtime '{name}' requires the runtime to call a guest function"
            )), offset));
        }
        Ok(())
    }

    pub(super) fn check_callback_params(
        &mut self,
        caller: InstanceId,
        target_view: &super::super::boundary::TemplateView<'h>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<()> {
        let fn_display = hir_compat::display_name(&target_view.name);
        for (param, &actual) in target_view.params.iter().zip(arg_tys) {
            let func_ident = match self.types.kind(actual) {
                TyKind::Func(id) => Some(id),
                _ => None,
            };
            let HirType::Function(expected) = &param.ty else {
                continue;
            };
            let Some(ident) = func_ident else {
                if !self.types.is_open(actual) {
                    return Err(self.hir.locations.annotate_call(
                        diag!(
                            codes::TYPE_MISMATCH,
                            "parameter '{}' of {fn_display} requires a statically known function",
                            param.name
                        ),
                        byte_offset,
                    ));
                }
                continue;
            };
            let cb_view = self.index.view(ident.template);
            let mismatch = || {
                self.hir.locations.annotate_call(
                    diag!(
                        codes::TYPE_MISMATCH,
                        "callback does not match parameter '{}' of {fn_display}",
                        param.name
                    ),
                    byte_offset,
                )
            };
            let required = cb_view
                .params
                .iter()
                .filter(|p| p.default.is_none() && !p.is_rest)
                .count();
            if (!cb_view.is_async && expected.is_async)
                || cb_view.is_generator != expected.is_generator
                || required > expected.params.len()
            {
                return Err(mismatch());
            }
            let mut expected_param_tys = Vec::with_capacity(expected.params.len());
            for (cb_p, (_, exp_p, _)) in cb_view.params.iter().zip(&expected.params) {
                let (exp_ty, cb_ty) = (
                    self.types.intern_hir(self.ctx, exp_p, true),
                    self.types.intern_hir(self.ctx, &cb_p.ty, true),
                );
                if !self.types.is_open(cb_ty) && !self.types.accepts(cb_ty, exp_ty) {
                    return Err(mismatch());
                }
                expected_param_tys.push(exp_ty);
            }
            let bound_cb = self.request_instance(
                InstanceReq::new(ident.template, &expected_param_tys, Some(caller))
                    .owner(ident.capture_owner)
                    .at(byte_offset),
            )?;
            let exp_ret = self.types.intern_hir(self.ctx, &expected.return_type, true);
            if exp_ret != Ty::VOID {
                self.result_contracts.insert(ResultContract {
                    caller,
                    parameter: param.id,
                    inst: bound_cb,
                    expected: exp_ret,
                    param_name: &param.name,
                    byte_offset,
                });
            }
        }
        Ok(())
    }

    pub(super) fn check_escaping_unannotated_func(
        &self,
        template: TemplateId,
        context_name: Option<&str>,
    ) -> Result<()> {
        let view = self.index.view(template);
        let owner_name = match context_name {
            Some(name) if view.name.is_empty() => hir_compat::display_name(name).to_owned(),
            _ => view.display_name(&self.hir.locations),
        };
        for param in view.params {
            if self.index.locals.rows[self.index.param_local(param)?]
                .flags
                .implicit_parameter(param)
            {
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "parameter '{}' of '{}' needs a type annotation",
                    param.name,
                    owner_name
                )
                .with_help("Add a concrete type annotation to this binding, parameter, or function return.");
                if let Some(span) = self.index.locals.row(param.id).and_then(|r| r.span) {
                    err = self.hir.locations.annotate(err, span);
                }
                return Err(err);
            }
        }
        Ok(())
    }
}
