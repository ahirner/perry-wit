//! Call, method and constructor resolution.
use super::*;
use crate::{methods::MethodResult, wit::MemberKind};

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
                self.types.substitute(self.ctx, t, &bindings)
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

    fn resource_operation(
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
        let ty = self.types.intern_hir(self.ctx, &member.result, true);
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
            TyKind::Leaf(id) => {
                if let HirType::Named(name) = id.hir(self.ctx) {
                    let nid = self.types.intern_name(name);
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
            let class_args = if !type_args.is_empty() {
                self.eval_type_args(s.inst, type_args)
            } else if !self.class_type_params[cid].is_empty() {
                let tps = self.class_type_params[cid].clone();
                let mut bindings = Vec::new();
                if let Some(ctor_tid) = self.index.class(cid).ctor {
                    let ctor_view = self.index.view(ctor_tid);
                    self.unify_params(ctor_view.params, &arg_tys, &tps, &mut bindings);
                }
                tps.iter()
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
            if let TyKind::Leaf(id) = self.types.kind(ty)
                && let HirType::Named(base) = id.hir(self.ctx)
                && let Some(owner) = self.decls.resource(base)
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
                let ret = self.types.intern_hir(self.ctx, &sig.return_type, true);
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
            let helper = self.intern_helper(HelperSpec::ThrowTypeError, byte_offset);
            self.record_site(s.inst, expr, SiteResolution::ThrowTypeError(helper));
            return Ok(Ty::NEVER);
        }
        if let Some(idents) = self.types.func_union_members(callee_ty) {
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
        let (stripped_callee, nullish) = self.types.strip_nullish(callee_ty);
        if nullish.is_some()
            && let TyKind::Func(ident) = self.types.kind(stripped_callee)
        {
            return self.eval_dispatch_call(
                s,
                expr,
                callee_ty,
                &[ident],
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

    pub(super) fn eval_method_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        object: &'h Expr,
        property: &str,
        args: &'h [Expr],
        byte_offset: u32,
    ) -> Result<Ty> {
        let method = hir_compat::method(property);
        let name = method.name();

        if let Expr::ExternFuncRef { name: class, .. } = object
            && let Some(owner) = self.decls.resource(class)
            && let Some(ty) = self.resource_operation(s.inst, expr, owner, MemberKind::Static, name)
        {
            self.eval_args(s, args)?;
            return Ok(ty);
        }
        if matches!(object, Expr::GlobalGet(_)) {
            match name {
                "log" | "info" | "debug" | "error" | "warn"
                    if matches!(object, Expr::GlobalGet(0)) =>
                {
                    self.eval_args(s, args)?;
                    return Ok(Ty::VOID);
                }
                "fetch" => {
                    self.eval_args(s, args)?;
                    let resp = self.types.named(self.ctx, "Response");
                    return Ok(self.types.promise(resp));
                }
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
                    return self.eval_timer_start(s, expr, name, args, byte_offset);
                }
                "clearTimeout" | "clearInterval" | "clearImmediate" => {
                    return self.eval_timer_clear(s, expr, args);
                }
                _ => {}
            }
        }
        if name == "toWeb" && let Expr::ExternFuncRef { name: local, .. } = object
            && self.hir.imports.iter().any(|imp| matches!(imp.source.as_str(), "stream" | "node:stream")
                && imp.specifiers.iter().any(|sp| matches!(sp, perry_hir::ir::ImportSpecifier::Named { imported, local: l } if imported == "Writable" && l == local)))
        {
            self.eval_args(s, args)?;
            return Ok(self.types.named(self.ctx, "WritableStream"));
        }
        if let Expr::PropertyGet {
            object: global,
            property: class,
            ..
        } = object
            && matches!(global.as_ref(), Expr::GlobalGet(_))
        {
            let mut arg_tys = self.eval_args(s, args)?;
            if class == "Promise" {
                for ty in &mut arg_tys {
                    *ty = self.resolve_promise_cells(s.inst, *ty);
                }
            }
            return Ok(builtins::static_global_method(
                self.ctx, self.types, class, name, args, &arg_tys,
            )
            .unwrap_or(Ty::OPEN));
        }
        if let Expr::PropertyGet {
            object: ns,
            property: class,
            ..
        } = object
            && let Expr::PropertyGet {
                object: global,
                property: ns_name,
                ..
            } = ns.as_ref()
            && matches!(global.as_ref(), Expr::GlobalGet(_))
            && ns_name == "Temporal"
        {
            self.eval_args(s, args)?;
            return Ok(
                builtins::temporal_static_method(self.ctx, self.types, class, property)
                    .unwrap_or(Ty::OPEN),
            );
        }
        if let Expr::ClassRef(class_name) = object {
            return self.eval_static_method_call(s, expr, class_name, name, args, byte_offset);
        }

        let obj_ty = self.eval_expr(s, object)?;
        if obj_ty == Ty::BOTTOM {
            self.eval_args(s, args)?;
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);

        if let TyKind::Leaf(id) = self.types.kind(stripped)
            && let HirType::Named(base) = id.hir(self.ctx)
            && let Some(owner) = self.decls.resource(base)
        {
            self.eval_args(s, args)?;
            if matches!(method, hir_compat::Method::UsingCheck)
                && let Expr::LocalGet(id) = object
            {
                let span = self.hir.local_source_spans.get(id).and_then(|s| {
                    self.metadata
                        .locations
                        .offset_span(s.start, s.end.saturating_sub(s.start))
                });
                return Err(Diagnostic::unsupported(
                    "using declarations for imported resources are unsupported",
                )
                .with_fallback_span(span));
            }
            if let Some(ty) =
                self.resource_operation(s.inst, expr, owner, MemberKind::Instance, name)
            {
                return Ok(ty);
            }
            return Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported(format!(
                    "method {name} is not supported on WIT resource yet"
                )),
                byte_offset,
            ));
        }
        self.check_timer_member(stripped, byte_offset)?;

        if matches!(name, "then" | "catch" | "finally")
            && matches!(
                self.types.kind(stripped),
                TyKind::Promise(_)
                    | TyKind::Cell {
                        kind: CellKind::Promise,
                        ..
                    }
            )
        {
            return self.eval_promise_reaction(s, expr, stripped, name, args, byte_offset);
        }

        let arg_tys = self.eval_args(s, args)?;

        if name == "set"
            && arg_tys.get(1).is_some_and(|&ty| self.types.has_func(ty))
            && builtins::receiver_of(self.ctx, self.types, stripped)
                == Some(crate::methods::Receiver::Map)
        {
            return Err(self.annotate_expr_error(
                s,
                Diagnostic::unsupported("storing a function value with Map.set is not supported"),
                byte_offset,
            ));
        }
        if matches!(name, "push" | "unshift") {
            let pushed_cell = self.push_into_cells(stripped, &arg_tys);
            self.coerce_array_write(s.inst, stripped, args, &arg_tys);
            if pushed_cell {
                return Ok(Ty::NUMBER);
            }
        }
        if method == hir_compat::Method::Iteration {
            return Ok(obj_ty);
        }

        let mname_id = self.types.intern_name(name);
        if let Some(cinst_id) = self.resolve_class_inst(s.inst, stripped, byte_offset)?
            && let Some(method_tid) = ClassInfo::find(
                &self.index.class(self.class_insts[cinst_id].class).methods,
                mname_id,
            )
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let view = self.index.view(method_tid);
            self.check_callback_params(s.inst, view, &arg_tys, byte_offset)?;
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(method_tid, &arg_tys, Some(s.inst))
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
                true,
                false,
            )?;
            return Ok(ret);
        }

        let prop_ty = if let Some(cinst_id) =
            self.resolve_class_inst(s.inst, stripped, byte_offset)?
            && let Some(loc) = self.class_field_loc(cinst_id, name)
        {
            Some(self.read_loc(loc, s.inst))
        } else {
            builtins::property_type(self.ctx, self.decls, self.types, stripped, name)
        };
        if let Some(prop_ty) = prop_ty
            && let Some(idents) = self.types.func_union_members(prop_ty)
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            return self.eval_dispatch_call(
                s,
                expr,
                prop_ty,
                &idents,
                Some(stripped),
                &arg_tys,
                byte_offset,
            );
        }
        if let Some(prop_ty) = prop_ty {
            let (stripped_prop, nullish) = self.types.strip_nullish(prop_ty);
            if nullish.is_some()
                && let TyKind::Func(ident) = self.types.kind(stripped_prop)
            {
                if arg_tys.contains(&Ty::BOTTOM) {
                    return Ok(Ty::BOTTOM);
                }
                return self.eval_dispatch_call(
                    s,
                    expr,
                    prop_ty,
                    &[ident],
                    Some(stripped),
                    &arg_tys,
                    byte_offset,
                );
            }
        }
        if let Some(prop_ty) = prop_ty
            && let TyKind::Func(ident) = self.types.kind(prop_ty)
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let method_view = self.index.view(ident.template);
            self.check_callback_params(s.inst, method_view, &arg_tys, byte_offset)?;
            let recv = method_view.uses_this.then_some(Receiver::Object(stripped));
            let pass_env = self.is_escaping_env(ident.template, s.inst);
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(ident.template, &arg_tys, Some(s.inst))
                    .receiver(recv)
                    .owner(ident.capture_owner)
                    .at(byte_offset),
                false,
                pass_env,
            )?;
            return Ok(ret);
        }

        if let Some(rule) =
            builtins::lookup_method_result(self.ctx, self.types, stripped, name, false)
        {
            let resolved_recv = if !matches!(
                rule,
                MethodResult::Number
                    | MethodResult::Boolean
                    | MethodResult::String
                    | MethodResult::Void
            ) {
                self.resolve_cell_read(s.inst, obj_ty)
            } else {
                obj_ty
            };
            if resolved_recv == Ty::BOTTOM {
                return Ok(Ty::BOTTOM);
            }
            return Ok(builtins::apply_method_result(
                self.ctx,
                self.types,
                rule,
                MethodResultArgs {
                    receiver: resolved_recv,
                    arg0: arg_tys.first().copied(),
                    arg0_expr: args.first(),
                    callback_result: None,
                    sparse_arrays: self.facts.sparse_arrays,
                },
            ));
        }
        let is_known_js_cb_method = matches!(
            name,
            "map"
                | "flatMap"
                | "filter"
                | "forEach"
                | "reduce"
                | "reduceRight"
                | "some"
                | "every"
                | "find"
                | "findIndex"
                | "sort"
                | "toSorted"
                | "replace"
                | "replaceAll"
                | "then"
                | "catch"
                | "finally"
                | "groupBy"
        );
        let is_collection = matches!(
            self.types.kind(stripped),
            TyKind::Array(_) | TyKind::Cell { .. }
        ) || matches!(self.types.kind(stripped), TyKind::Generic { base, .. }
            if matches!(base.hir(self.ctx), HirType::Named(n) if matches!(n.as_str(), "Map" | "Set" | "ReadonlyMap" | "ReadonlySet")));
        let skip_host_cb = is_collection || (self.types.is_open(stripped) && is_known_js_cb_method);
        if hir_compat::has_source_offset(byte_offset)
            && !skip_host_cb
            && arg_tys.iter().any(|&a| self.types.has_func(a))
        {
            return Err(self.metadata.locations.annotate_call(
                Diagnostic::unsupported(format!(
                    "passing a callback to runtime '{name}' requires the runtime to call a guest function"
                )),
                byte_offset,
            ));
        }
        Ok(Ty::OPEN)
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_dispatch_call(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
        callee_ty: Ty,
        idents: &[FuncIdentity],
        recv_ty: Option<Ty>,
        arg_tys: &[Ty],
        byte_offset: u32,
    ) -> Result<Ty> {
        for &a in arg_tys {
            self.invalidate_conservative_cell(a);
        }
        let mut arms = Vec::with_capacity(idents.len());
        let mut ret = Ty::BOTTOM;
        for &ident in idents {
            let view = self.index.view(ident.template);
            let is_promise_handle =
                self.apply_promise_handle_call(s, ident, arg_tys, None, byte_offset)?;
            self.check_callback_params(s.inst, view, arg_tys, byte_offset)?;
            let recv = recv_ty.filter(|_| view.uses_this).map(Receiver::Object);
            let call_arg_tys = if is_promise_handle {
                arg_tys.get(..1).unwrap_or(&[])
            } else {
                arg_tys
            };
            let target = self.request_instance(
                InstanceReq::new(ident.template, call_arg_tys, Some(s.inst))
                    .receiver(recv)
                    .owner(ident.capture_owner)
                    .at(byte_offset),
            )?;
            self.propagate_writes(s.inst, target);
            let r = self.read_loc(self.instances[target].return_loc, s.inst);
            ret = self.types.join(ret, r);
            arms.push((ident, target));
        }
        let arms_span = self.intern_dispatch_arms(&arms);
        let arg_tuple = self.types.tuple(arg_tys);
        let needs_recv = recv_ty.is_some()
            && arms
                .iter()
                .any(|&(ident, _)| self.index.view(ident.template).uses_this);
        let helper = self.intern_helper(
            HelperSpec::Dispatcher {
                callee_ty,
                arms: arms_span,
                arg_tys: arg_tuple,
                recv_ty: needs_recv.then_some(recv_ty).flatten(),
            },
            byte_offset,
        );
        self.record_site(
            s.inst,
            expr,
            SiteResolution::DispatchCall {
                helper,
                argument_types: arg_tuple,
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
                if !self.types.is_open(cb_ty) && !self.types.accepts(self.ctx, cb_ty, exp_ty) {
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
