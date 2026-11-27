//! Call, method and constructor resolution.
use super::*;
use crate::engine::types::WitView;

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
            let erased = self
                .types
                .is_zero_sized_func(ty, &self.index.capture_free_templates);
            self.site_mut(s.inst, expr)?.operand = if erased {
                OperandUse::Erased
            } else {
                OperandUse::Value
            };
            let ty = self.types.normalize_for_storage(self.ctx, self.decls, ty);
            props.push(self.types.property(name, ty, false));
        }
        if props.iter().any(|p| p.ty == Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        Ok(self.types.object(&props, None))
    }

    pub(super) fn resource_result(&mut self, member: crate::wit::MemberId) -> Ty {
        self.decls.world.members[member]
            .result
            .map_or(Ty::VOID, |value| {
                self.types.wit_type(self.ctx, value, WitView::Reference)
            })
    }

    pub(super) fn eval_static_method_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        class_name: &str,
        method_name: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let arg_tys = self.eval_args(s, args)?;
        if arg_tys.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        let class = self.types.intern_name(class_name);
        if let Some(cid) = self.index.class_by_name_id(class)
            && let Some(tid) = self.index.method_template(
                &self.index.class(cid).declaration.static_methods,
                hir_compat::method(method_name).name(),
            )
        {
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(tid, &arg_tys, Some(s.inst)).at(byte_offset),
                false,
            )?;
            return Ok(ret);
        }
        Ok(Ty::OPEN)
    }

    pub(super) fn dispatch_call_site(
        &mut self,
        inst_id: InstanceId,
        expr: &'h Expr,
        mut req: InstanceReq<'_>,
        pass_env: bool,
    ) -> Result<(InstanceId, Ty)> {
        let view = self.index.view(req.template);
        if view.kind.is_class_member() && !view.params.last().is_some_and(|p| p.is_rest) {
            req.args = &req.args[..req.args.len().min(view.params.len())];
        }
        let arg_types = req.args;
        let pass_receiver = req.receiver.is_some();
        let target = self.request_instance(req)?;
        let res = SiteResolution::Call {
            target: CallTarget::Direct {
                target,
                pass_receiver,
                pass_env,
            },
            argument_types: self.types.tuple(arg_types),
        };
        self.record_site(inst_id, expr, res);
        let return_loc = self.instances[target].return_loc;
        Ok((target, self.read_loc(return_loc, inst_id)))
    }

    pub(super) fn resolve_class_inst(
        &mut self,
        inst_id: InstanceId,
        stripped: Ty,
        byte_offset: u32,
    ) -> Result<Option<ClassInstId>> {
        match self.types.kind(stripped) {
            TyKind::ClassInst(id) => Ok(Some(id)),
            TyKind::Named(id) => {
                {
                    let nid = self.types.intern_name(id.get(self.ctx));
                    if let Some(cid) = self.index.class_by_name_id(nid) {
                        return Ok(Some(self.request_class_inst(
                            cid,
                            &[],
                            byte_offset,
                            Some(inst_id),
                        )?));
                    }
                }
                Ok(None)
            }
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
        if let Some(&fields) = self.index.literal_classes.get(class_name) {
            return self.eval_object_fields(
                s,
                fields.iter().zip(args).map(|(f, a)| (f.name.as_str(), a)),
            );
        }
        let cname_id = self.types.intern_name(class_name);
        if class_name == "Promise" && self.index.class_by_name_id(cname_id).is_none() {
            return self.eval_new_promise(s, expr, args, type_args, byte_offset);
        }
        let arg_tys = self.eval_args(s, args)?;
        if let Some(cid) = self.index.class_by_name_id(cname_id) {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let explicit = self.eval_type_args(s.inst, type_args);
            let class_inst =
                self.construct_class(cid, &arg_tys, explicit, byte_offset, Some(s.inst))?;
            for &ty in &arg_tys {
                self.mark_escaping_env(ty);
            }
            let arguments = self.types.tuple(&arg_tys);
            self.record_site(
                s.inst,
                expr,
                SiteResolution::ClassNew(class_inst, arguments),
            );
            return Ok(self.types.class_inst(class_inst));
        }
        if arg_tys.iter().any(|&a| self.types.has_func(a)) {
            return Err(self.hir.locations.annotate_call(
                Diagnostic::unsupported(format!(
                    "passing a callback to runtime '{class_name}' requires the runtime to call a guest function"
                )),
                byte_offset,
            ));
        }
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
        } else if self.index.class_has_implicit_params.contains(cid.index()) {
            arg_tys.to_vec()
        } else {
            Vec::new()
        };
        self.request_class_inst(cid, &class_args, byte_offset, caller)
    }

    pub(super) fn eval_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        if matches!(callee, Expr::ExternFuncRef { name, .. } if name == "structuredClone") {
            return match args.first() {
                Some(a) => self.eval_expr(s, a),
                None => Ok(Ty::OPEN),
            };
        }
        if let Expr::ExternFuncRef {
            name, return_type, ..
        } = callee
        {
            use crate::shapes::Imported;
            let binding = self.decls.imports.get(name.as_str()).copied();
            let builtin = match binding {
                Some(Imported::Timer(imported)) => Some(imported),
                None => Some(name.as_str()),
                _ => None,
            };
            if let Some(builtin) = builtin {
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
            let arg_tys = self.eval_args(s, args)?;
            match binding {
                Some(Imported::ReadFile) => {
                    let text = hir_compat::read_file_text(args.get(1), &self.index.literal_classes)
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
                Some(Imported::Foreign(_, sig)) => {
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
            if binding.is_none()
                && hir_compat::is_source_extern(name, byte_offset)
                && arg_tys.iter().any(|&a| self.types.has_func(a))
            {
                return Err(self.hir.locations.annotate_call(
                    Diagnostic::unsupported(format!(
                        "passing a callback to runtime '{name}' requires the runtime to call a guest function"
                    )),
                    byte_offset,
                ));
            }
            return Ok(self.types.intern_hir(self.ctx, return_type, true));
        }
        if let Some((object, property)) = self.index.receiver_members.method(callee) {
            if let Some(property) = property {
                return self.eval_method_call(s, expr, object, property, args, byte_offset);
            }
            let ty = self.eval_expr(s, object)?;
            if ty == Ty::BOTTOM {
                return Ok(ty);
            }
            if let TyKind::Named(name) = self.types.kind(ty)
                && let Some(owner) = self.decls.resource(name.get(self.ctx))
                && let Some(member) = self.index.receiver_members.get(object, owner)
            {
                self.eval_args(s, args)?;
                self.record_site(s.inst, expr, SiteResolution::ResourceCall(member));
                return Ok(Ty::VOID);
            }
        }

        let callee_ty = self.eval_expr(s, callee)?;
        let arg_tys = self.eval_args(s, args)?;
        self.eval_callable(s, expr, callee_ty, None, &arg_tys)
    }

    pub(super) fn eval_callable(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee_ty: Ty,
        receiver: Option<Ty>,
        arg_tys: &[Ty],
    ) -> Result<Ty> {
        let (args, type_args, byte_offset) = match expr {
            Expr::Call {
                args,
                type_args,
                byte_offset,
                ..
            } => (Some(args.as_slice()), type_args, *byte_offset),
            Expr::CallSpread {
                type_args,
                byte_offset,
                ..
            } => (None, type_args, *byte_offset),
            _ => return Err(Diagnostic::internal("call site requires a call expression")),
        };
        if callee_ty == Ty::BOTTOM || arg_tys.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        if matches!(callee_ty, Ty::NEVER | Ty::NULL | Ty::VOID) {
            let argument_types = self.types.tuple(arg_tys);
            self.record_site(
                s.inst,
                expr,
                SiteResolution::Call {
                    target: CallTarget::ThrowTypeError,
                    argument_types,
                },
            );
            return Ok(Ty::NEVER);
        }
        if let Some(idents) = self.types.func_dispatch_members(callee_ty, true) {
            return self.eval_dispatch_call(
                s,
                expr,
                callee_ty,
                &idents,
                receiver,
                arg_tys,
                byte_offset,
            );
        }
        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            if !self.types.is_open(callee_ty) && callee_ty != Ty::DYNAMIC {
                return Err(attach_inst_chain(
                    &self.instances,
                    self.index,
                    &self.hir.locations,
                    self.hir.locations.annotate_call(
                        diag!(codes::TYPE_MISMATCH, "expression is not callable"),
                        byte_offset,
                    ),
                    s.inst,
                ));
            }
            return Ok(Ty::OPEN);
        };
        for &a in arg_tys {
            self.invalidate_conservative_cell(a);
        }
        let target_view = self.index.view(ident.template);
        let is_promise_handle =
            self.apply_promise_handle_call(s, ident, arg_tys, args, byte_offset)?;

        if receiver.is_none()
            && args.into_iter().flatten().any(|a| {
                matches!(
                    a,
                    Expr::Closure {
                        captures_this: true,
                        ..
                    } | Expr::Closure {
                        captures_new_target: true,
                        ..
                    }
                )
            })
        {
            return Err(self.hir.locations.annotate_call(
                Diagnostic::unsupported(
                    "a callback that captures this or new.target requires its original receiver",
                ),
                byte_offset,
            ));
        }
        if args.is_some()
            && receiver.is_none()
            && !is_promise_handle
            && !target_view.params.iter().any(|p| p.is_rest)
            && arg_tys.len() > target_view.params.len()
        {
            self.check_escaping_unannotated_func(ident.template, None)?;
        }
        self.check_callback_params(s.inst, target_view, arg_tys, byte_offset)?;

        let explicit_targs = self.eval_type_args(s.inst, type_args);
        let pass_env = self.is_escaping_env(ident.template, s.inst);
        let call_arg_tys = if is_promise_handle {
            arg_tys.get(..1).unwrap_or(&[])
        } else {
            arg_tys
        };
        let (target_inst, ret) = self.dispatch_call_site(
            s.inst,
            expr,
            InstanceReq::new(ident.template, call_arg_tys, Some(s.inst))
                .receiver(
                    receiver
                        .filter(|_| target_view.uses_this)
                        .map(Receiver::Object),
                )
                .owner(ident.capture_owner)
                .type_args(&explicit_targs)
                .at(byte_offset),
            pass_env,
        )?;
        self.propagate_writes(s.inst, target_inst);
        Ok(ret)
    }

    pub(super) fn eval_call_spread(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [perry_hir::ir::CallArg],
    ) -> Result<Ty> {
        let (callee_ty, receiver, builtin_result) = if let Expr::PropertyGet {
            object,
            property,
            byte_offset,
            ..
        } = callee
        {
            let ty = self.eval_expr(s, object)?;
            let (ty, _) = self.types.strip_nullish(ty);
            let name = hir_compat::method(property).name();
            if name == "push" && self.type_may_be_array(ty) {
                for (value, spread) in crate::source::Arguments::Spread(args) {
                    let value_ty = self.eval_expr(s, value)?;
                    self.push_argument(s.inst, ty, value, value_ty, spread);
                }
                return Ok(Ty::NUMBER);
            }
            let callable = self.method_property(s, ty, name, *byte_offset)?;
            let fallback = match name {
                "concat" | "filter" | "sort" | "toSorted" => ty,
                "max" | "min" => Ty::NUMBER,
                _ => Ty::OPEN,
            };
            (callable, Some(ty), fallback)
        } else {
            (Some(self.eval_expr(s, callee)?), None, Ty::OPEN)
        };
        let mut expanded = Vec::with_capacity(args.len());
        for (expr, spread) in crate::source::Arguments::Spread(args) {
            let ty = self.eval_expr(s, expr)?;
            if spread && ty != Ty::BOTTOM {
                let (ty, _) = self.types.strip_nullish(ty);
                let ty = self.resolve_cell_read(s.inst, ty);
                self.site_mut(s.inst, expr)?.operand = OperandUse::Spread(ty);
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
        match callee_ty {
            Some(ty) if receiver.is_none() || self.types.has_func(ty) => {
                self.eval_callable(s, expr, ty, receiver, &expanded)
            }
            _ => Ok(builtin_result),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn eval_dispatch_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee_ty: Ty,
        idents: &[FuncIdentity],
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<Ty> {
        let (target, argument_types, ret) = self.callback_dispatcher(
            s,
            callee_ty,
            idents,
            arg_tys,
            callbacks::DispatchSite::Call(recv_ty, byte_offset),
        )?;
        self.record_site(
            s.inst,
            expr,
            SiteResolution::Call {
                target,
                argument_types,
            },
        );
        Ok(ret)
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
            if !self.index.is_param_annotated(param.id)
                && matches!(param.ty, HirType::Any | HirType::Unknown)
            {
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "parameter '{}' of '{}' needs a type annotation",
                    param.name,
                    owner_name
                )
                .with_help("Add a concrete type annotation to this binding, parameter, or function return.");
                if let Some(span) = self.hir.declarations.locals.get(&param.id) {
                    err = self.hir.locations.annotate(err, span.span);
                }
                return Err(err);
            }
        }
        Ok(())
    }
}
