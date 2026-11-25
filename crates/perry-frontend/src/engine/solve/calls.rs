//! Call, method and constructor resolution.
use super::*;
use crate::wit::MemberKind;

impl<'h, 'a> Solver<'h, 'a> {
    pub(super) fn eval_args(&mut self, s: &mut EvalState<'h>, args: &'h [Expr]) -> Result<Vec<Ty>> {
        args.iter().map(|a| self.eval_expr(s, a)).collect()
    }

    pub(super) fn eval_type_args(&mut self, inst_id: InstanceId, type_args: &[HirType]) -> Vec<Ty> {
        if type_args.is_empty() {
            return Vec::new();
        }
        let bindings = self.active_instance_type_bindings(inst_id);
        type_args
            .iter()
            .map(|ta| {
                let t = self.types.intern_hir(self.ctx, ta, true);
                self.types.substitute(t, &bindings)
            })
            .collect()
    }

    pub(super) fn eval_object_fields<'b>(
        &mut self,
        s: &mut EvalState<'h>,
        site_expr: &'h Expr,
        pairs: impl Iterator<Item = (&'b str, &'h Expr)>,
    ) -> Result<Ty> {
        let mut props = Vec::new();
        let mut has_zero_sized_func = false;
        for (name, expr) in pairs {
            let ty = self.eval_expr(s, expr)?;
            self.invalidate_conservative_cell(ty);
            self.check_escaping_func(s.inst, ty, 0, s.current_span)?;
            if self
                .types
                .is_zero_sized_func(ty, &self.index.capture_free_templates)
            {
                has_zero_sized_func = true;
            }
            let ty = self.types.normalize_for_storage(self.ctx, self.decls, ty);
            props.push(ObjectProp {
                name: self.types.intern_name(name),
                ty,
                optional: false,
                readonly: false,
            });
        }
        if props.iter().any(|p| p.ty == Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        let obj_ty = self.types.object(&props, None);
        if has_zero_sized_func {
            self.record_site(s.inst, site_expr, SiteResolution::ObjectLiteral(obj_ty));
        }
        Ok(obj_ty)
    }

    pub(super) fn resource_operation(
        &mut self,
        inst: InstanceId,
        expr: &Expr,
        owner: u32,
        kind: MemberKind,
        name: &str,
    ) -> Option<Ty> {
        let (id, member) = self
            .metadata
            .resource_members
            .iter()
            .enumerate()
            .find(|(_, m)| {
                m.owner == owner
                    && m.kind == kind
                    && (kind == MemberKind::Constructor || m.name == name)
            })?;
        let ty = member.result.map_or(Ty::VOID, |value| {
            self.types.wit_type(self.ctx, value, false)
        });
        self.record_site(inst, expr, SiteResolution::ResourceCall(id));
        Some(ty)
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
        let (cn, mn) = (
            self.types.intern_name(class_name),
            self.types
                .intern_name(hir_compat::method(method_name).name()),
        );
        if let Some(tid) = self
            .index
            .class_by_name_id(cn)
            .and_then(|cid| ClassInfo::find(&self.index.class(cid).static_methods, mn))
        {
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(tid, &arg_tys, Some(s.inst)).at(byte_offset),
                true,
                false,
            )?;
            return Ok(ret);
        }
        if let Some(owner) = self.decls.resource(class_name)
            && let Some(ty) =
                self.resource_operation(s.inst, expr, owner, MemberKind::Static, method_name)
        {
            return Ok(ty);
        }
        Ok(Ty::OPEN)
    }

    pub(super) fn dispatch_call_site(
        &mut self,
        inst_id: InstanceId,
        expr: &'h Expr,
        req: InstanceReq<'_>,
        is_method: bool,
        pass_env: bool,
    ) -> Result<(InstanceId, Ty)> {
        let argument_types = self.types.tuple(req.args);
        let pass_receiver = matches!(req.receiver, Some(Receiver::Object(_)));
        let target = self.request_instance(req)?;
        let res = if is_method {
            SiteResolution::MethodCall {
                target,
                argument_types,
            }
        } else {
            SiteResolution::DirectCall {
                target,
                argument_types,
                pass_receiver,
                pass_env,
            }
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
        let cname_id = self.types.intern_name(class_name);
        if let Some(fields) = self.index.literal_fields_by_id(cname_id) {
            return self.eval_object_fields(
                s,
                expr,
                fields.iter().zip(args).map(|(f, a)| (f.name.as_str(), a)),
            );
        }
        if class_name == "Promise" && self.index.class_by_name_id(cname_id).is_none() {
            return self.eval_new_promise(s, expr, args, type_args, byte_offset);
        }
        let arg_tys = self.eval_args(s, args)?;
        if let Some(cid) = self.index.class_by_name_id(cname_id) {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let tp_span = self.class_type_params[cid];
            let class_args = if !type_args.is_empty() {
                self.eval_type_args(s.inst, type_args)
            } else if !self.type_param_pool[tp_span].is_empty() {
                let mut bindings = Vec::new();
                if let Some(ctor_tid) = self.index.class(cid).ctor {
                    let ctor_view = self.index.view(ctor_tid);
                    self.unify_params(ctor_view.params, &arg_tys, tp_span, &mut bindings);
                }
                self.type_param_pool[tp_span]
                    .iter()
                    .map(|tp| {
                        bindings
                            .iter()
                            .find(|(k, _)| k == tp)
                            .map_or(Ty::OPEN, |(_, v)| *v)
                    })
                    .collect()
            } else if self.index.class_has_implicit_params.contains(cid.index()) {
                arg_tys.clone()
            } else {
                Vec::new()
            };
            let class_inst =
                self.request_class_inst(cid, &class_args, byte_offset, Some(s.inst))?;
            let argument_types = self.types.tuple(&arg_tys);
            self.record_site(
                s.inst,
                expr,
                SiteResolution::ClassNew {
                    class_inst,
                    argument_types,
                },
            );
            return Ok(self.types.class_inst(class_inst));
        }
        if let Some(owner) = self.decls.resource(class_name)
            && let Some(ty) =
                self.resource_operation(s.inst, expr, owner, MemberKind::Constructor, "")
        {
            return Ok(ty);
        }
        if arg_tys.iter().any(|&a| self.types.has_func(a)) {
            return Err(self.metadata.locations.annotate_call(
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

    pub(super) fn eval_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee: &'h Expr,
        args: &'h [Expr],
        type_args: &'h [HirType],
        byte_offset: u32,
    ) -> Result<Ty> {
        if let Expr::IndexGet { object, .. } = callee
            && let Expr::Call { byte_end, .. } = expr
            && self
                .metadata
                .locations
                .calls_dispose(byte_offset, *byte_end)
        {
            let ty = self.eval_expr(s, object)?;
            if ty == Ty::BOTTOM {
                return Ok(Ty::BOTTOM);
            }
            if let TyKind::Named(id) = self.types.kind(ty)
                && let Some(owner) = self.decls.resource(id.get(self.ctx))
                && let Some(result) =
                    self.resource_operation(s.inst, expr, owner, MemberKind::Dispose, "")
            {
                self.eval_args(s, args)?;
                return Ok(result);
            }
        }
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
            use perry_hir::ir::ImportSpecifier;
            let binding = self.hir.imports.iter().find_map(|import| {
                import.specifiers.iter().find_map(|specifier| {
                    let local = match specifier {
                        ImportSpecifier::Named { local, .. }
                        | ImportSpecifier::Default { local }
                        | ImportSpecifier::Namespace { local } => local,
                    };
                    (local == name).then_some((import.source.as_str(), specifier))
                })
            });
            let named = binding.and_then(|(source, specifier)| match specifier {
                ImportSpecifier::Named { imported, .. } => Some((source, imported.as_str())),
                _ => None,
            });
            let builtin = match named {
                Some((source, imported)) if hir_compat::is_timer_module(source) => Some(imported),
                _ if binding.is_none() => Some(name.as_str()),
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
            if let Some((source, imported)) = named
                && let Some(res) = self.eval_builtin_import_call(source, imported, args, &arg_tys)
            {
                return Ok(res);
            }
            if binding.is_none()
                && hir_compat::is_source_extern(name, byte_offset)
                && arg_tys.iter().any(|&a| self.types.has_func(a))
            {
                return Err(self.metadata.locations.annotate_call(
                    Diagnostic::unsupported(format!(
                        "passing a callback to runtime '{name}' requires the runtime to call a guest function"
                    )),
                    byte_offset,
                ));
            }
            if let Some((_, sig)) = self.decls.foreign_functions.get(name.as_str()) {
                let ret = sig
                    .result
                    .map_or(Ty::VOID, |ty| self.types.wit_type(self.ctx, ty, false));
                return Ok(
                    if sig.is_async && !matches!(self.types.kind(ret), TyKind::Promise(_)) {
                        self.types.promise(ret)
                    } else {
                        ret
                    },
                );
            }
            return Ok(self.types.intern_hir(self.ctx, return_type, true));
        }
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            return self.eval_method_call(s, expr, object, property, args, byte_offset);
        }

        let callee_ty = self.eval_expr(s, callee)?;
        let arg_tys = self.eval_args(s, args)?;
        if callee_ty == Ty::BOTTOM || arg_tys.contains(&Ty::BOTTOM) {
            return Ok(Ty::BOTTOM);
        }
        if matches!(callee_ty, Ty::NEVER | Ty::NULL | Ty::VOID) {
            let helper = self.intern_helper(
                HelperSpec::Callable(CallableOp::ThrowTypeError),
                byte_offset,
            );
            self.record_site(s.inst, expr, SiteResolution::ThrowTypeError(helper));
            return Ok(Ty::NEVER);
        }
        if let Some(idents) = self.types.func_dispatch_members(callee_ty, true) {
            return self.eval_dispatch_call(
                s,
                expr,
                callee_ty,
                &idents,
                None,
                &arg_tys,
                byte_offset,
            );
        }
        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            if !self.types.is_open(callee_ty) && callee_ty != Ty::DYNAMIC {
                return Err(attach_inst_chain(
                    &self.instances,
                    self.index,
                    &self.metadata.locations,
                    self.metadata.locations.annotate_call(
                        diag!(codes::TYPE_MISMATCH, "expression is not callable"),
                        byte_offset,
                    ),
                    s.inst,
                ));
            }
            return Ok(Ty::OPEN);
        };
        for &a in &arg_tys {
            self.invalidate_conservative_cell(a);
        }
        let target_view = self.index.view(ident.template);
        let is_promise_handle =
            self.apply_promise_handle_call(s, ident, &arg_tys, Some(args), byte_offset)?;

        if args.iter().any(|a| {
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
        }) {
            return Err(self.metadata.locations.annotate_call(
                Diagnostic::unsupported(
                    "a callback that captures this or new.target requires its original receiver",
                ),
                byte_offset,
            ));
        }
        if !is_promise_handle
            && !target_view.params.iter().any(|p| p.is_rest)
            && arg_tys.len() > target_view.params.len()
        {
            self.check_escaping_unannotated_func(ident.template, None)?;
        }
        self.check_callback_params(s.inst, target_view, &arg_tys, byte_offset)?;

        let explicit_targs = self.eval_type_args(s.inst, type_args);
        let pass_env = self.is_escaping_env(ident.template, s.inst);
        let call_arg_tys = if is_promise_handle {
            arg_tys.get(..1).unwrap_or(&[])
        } else {
            &arg_tys
        };
        let (target_inst, ret) = self.dispatch_call_site(
            s.inst,
            expr,
            InstanceReq::new(ident.template, call_arg_tys, Some(s.inst))
                .owner(ident.capture_owner)
                .type_args(&explicit_targs)
                .at(byte_offset),
            false,
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
        if let Expr::PropertyGet {
            object, property, ..
        } = callee
        {
            let obj_ty = self.eval_expr(s, object)?;
            for arg in args {
                let (perry_hir::ir::CallArg::Expr(e) | perry_hir::ir::CallArg::Spread(e)) = arg;
                self.eval_expr(s, e)?;
            }
            return Ok(match hir_compat::method(property).name() {
                "concat" | "filter" | "sort" | "toSorted" => obj_ty,
                "max" | "min" => Ty::NUMBER,
                _ => Ty::OPEN,
            });
        }
        let callee_ty = self.eval_expr(s, callee)?;
        let mut expanded = Vec::with_capacity(args.len());
        let mut has_bottom = callee_ty == Ty::BOTTOM;
        for arg in args {
            let (e, is_spread) = match arg {
                perry_hir::ir::CallArg::Expr(e) => (e, false),
                perry_hir::ir::CallArg::Spread(e) => (e, true),
            };
            let ty = self.eval_expr(s, e)?;
            if ty == Ty::BOTTOM {
                has_bottom = true;
                continue;
            }
            if is_spread {
                let (stripped, _) = self.types.strip_nullish(ty);
                match self.types.kind(stripped) {
                    TyKind::Tuple(span) => expanded.extend_from_slice(self.types.ty_slice(span)),
                    _ => expanded.push(
                        builtins::iterated_element(self.ctx, self.types, stripped)
                            .unwrap_or(Ty::OPEN),
                    ),
                }
            } else {
                expanded.push(ty);
            }
        }
        if has_bottom {
            return Ok(Ty::BOTTOM);
        }
        let TyKind::Func(ident) = self.types.kind(callee_ty) else {
            return Ok(Ty::OPEN);
        };
        for &a in &expanded {
            self.invalidate_conservative_cell(a);
        }
        let pass_env = self.is_escaping_env(ident.template, s.inst);
        let (_, ret) = self.dispatch_call_site(
            s.inst,
            expr,
            InstanceReq::new(ident.template, &expanded, Some(s.inst)).owner(ident.capture_owner),
            false,
            pass_env,
        )?;
        Ok(ret)
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
        let (helper, argument_types, needs_recv, ret) =
            self.build_callback_dispatcher(s, callee_ty, idents, recv_ty, arg_tys, byte_offset)?;
        self.record_site(
            s.inst,
            expr,
            SiteResolution::DispatchCall {
                helper,
                argument_types,
                needs_recv,
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
        let fn_display = hir_compat::display_name(target_view.name);
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
                    return Err(self.metadata.locations.annotate_call(
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
                self.metadata.locations.annotate_call(
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
                self.contract_insts.insert(bound_cb.index());
                self.result_contracts.push(ResultContract {
                    inst: bound_cb,
                    expected: exp_ret,
                    param_name: param.name.clone(),
                    byte_offset,
                });
            }
        }
        Ok(())
    }

    fn eval_builtin_import_call(
        &mut self,
        source: &str,
        imported: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        match (source, imported) {
            ("timers/promises" | "node:timers/promises", "setTimeout") => {
                match arg_tys.get(1).copied() {
                    Some(Ty::BOTTOM) => Some(Ty::BOTTOM),
                    other => Some(self.types.promise(other.unwrap_or(Ty::VOID))),
                }
            }
            ("fs/promises" | "node:fs/promises", "readFile") => {
                let enc = args.get(1).is_some_and(|a| {
                    matches!(a, Expr::String(_))
                        || matches!(a, Expr::Object(p) if p.iter().any(|(k, _)| k == "encoding"))
                });
                let inner = if enc {
                    Ty::STRING
                } else {
                    self.types.named(self.ctx, "Uint8Array")
                };
                Some(self.types.promise(inner))
            }
            ("fs/promises" | "node:fs/promises", "stat") => {
                let s = self.types.named(self.ctx, "Stats");
                Some(self.types.promise(s))
            }
            ("fs/promises" | "node:fs/promises", "readdir") => {
                let a = self.types.array(Ty::STRING);
                Some(self.types.promise(a))
            }
            ("fs/promises" | "node:fs/promises", "writeFile" | "mkdir" | "unlink" | "rmdir") => {
                Some(self.types.promise(Ty::VOID))
            }
            ("perry:http-server", "request") => Some(self.types.named(self.ctx, "__PerryIncoming")),
            ("perry:http-server", "body") => {
                let b = self.types.named(self.ctx, "Uint8Array");
                Some(self.types.promise(b))
            }
            ("perry:http-server", "method" | "scheme" | "headerName") => Some(Ty::STRING),
            ("perry:http-server", "methodOther" | "schemeOther" | "authority" | "path") => {
                Some(self.types.with_absences(Ty::STRING, Absence::Undefined))
            }
            ("perry:http-server", "hasScheme") => Some(Ty::BOOLEAN),
            ("perry:http-server", "headerCount" | "respond") => Some(Ty::NUMBER),
            ("perry:http-server", "headerValue") => Some(self.types.named(self.ctx, "Uint8Array")),
            ("perry:http-server", "addHeader") => Some(Ty::VOID),
            _ => None,
        }
    }

    pub(super) fn check_escaping_unannotated_func(
        &self,
        template: TemplateId,
        context_name: Option<&str>,
    ) -> Result<()> {
        let view = self.index.view(template);
        let owner_name = match context_name {
            Some(name) if view.name.is_empty() => hir_compat::display_name(name).to_owned(),
            _ => view.display_name(),
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
                if let Some(span) = self.hir.local_source_spans.get(&param.id) {
                    err = self.metadata.locations.annotate(
                        err,
                        swc_common::Span::new(
                            swc_common::BytePos(span.start),
                            swc_common::BytePos(span.end),
                        ),
                    );
                }
                return Err(err);
            }
        }
        Ok(())
    }
}
