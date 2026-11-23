//! Call, method, constructor, built-in callback, and host-import resolution.
use super::*;
use crate::methods::{Callback as Cb, Method, MethodResult};

struct CallbackSite<'h> {
    callback: &'h Expr,
    inputs: Vec<Ty>,
    rule: MethodResult,
    recv_ty: Ty,
    init_ty: Option<Ty>,
    sparse_arrays: bool,
    group_elem: Option<Ty>,
}

impl<'h> CallbackSite<'h> {
    fn new(callback: &'h Expr, inputs: Vec<Ty>, rule: MethodResult, recv_ty: Ty) -> Self {
        Self {
            callback,
            inputs,
            rule,
            recv_ty,
            init_ty: None,
            sparse_arrays: false,
            group_elem: None,
        }
    }
}

impl<'h, 'a> Solver<'h, 'a> {
    fn array_cb_inputs(&mut self, kind: Cb, recv_ty: Ty, elem: Ty) -> Vec<Ty> {
        match kind {
            Cb::Elements => vec![elem, Ty::NUMBER, recv_ty],
            Cb::AllIndices => vec![
                self.types.with_absences(elem, Absence::Undefined),
                Ty::NUMBER,
                recv_ty,
            ],
            Cb::Comparator => {
                let d = self.types.exclude_absences(elem, Absence::Undefined);
                vec![d, d]
            }
            _ => Vec::new(),
        }
    }

    fn match_builtin_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Option<Result<CallbackSite<'h>, Ty>>> {
        let group_by = match expr {
            Expr::MapGroupBy { items, key_fn } => Some((items.as_ref(), key_fn.as_ref())),
            Expr::Call { callee, args, .. }
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref()
                    && let Expr::PropertyGet {
                        object: g,
                        property: cls,
                        ..
                    } = object.as_ref()
                    && matches!(g.as_ref(), Expr::GlobalGet(_))
                    && cls == "Map"
                    && hir_compat::method(property).name() == "groupBy"
                    && let [items, key_fn] = args.as_slice() =>
            {
                Some((items, key_fn))
            }
            _ => None,
        };
        if let Some((items, key_fn)) = group_by {
            let src = self.eval_expr(s, items)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            let elem = builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN);
            let mut site =
                CallbackSite::new(key_fn, vec![elem, Ty::NUMBER], MethodResult::Void, src);
            site.group_elem = Some(elem);
            return Ok(Some(Ok(site)));
        }

        if let Expr::ArrayFromMapped {
            iterable, map_fn, ..
        } = expr
        {
            let src = self.eval_expr(s, iterable)?;
            if src == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            let len_name = self.types.intern_name("length");
            let src_def = self.types.strip_nullish(src).0;
            let is_array_like = match self.types.kind(src_def) {
                TyKind::Object { props, .. } => self
                    .types
                    .object_slice(props)
                    .iter()
                    .any(|p| p.name == len_name),
                _ => {
                    matches!(iterable.as_ref(), Expr::Object(p) if p.iter().any(|(k, _)| k == "length"))
                }
            };
            let elem = if is_array_like {
                Ty::VOID
            } else {
                builtins::iterated_element(self.ctx, self.types, src).unwrap_or(Ty::OPEN)
            };
            return Ok(Some(Ok(CallbackSite::new(
                map_fn.as_ref(),
                vec![elem, Ty::NUMBER],
                MethodResult::Mapped,
                src,
            ))));
        }

        let (object, method_name, args): (&'h Expr, &str, &'h [Expr]) = match expr {
            Expr::ArrayMap { array, callback } => (array, "map", std::slice::from_ref(callback)),
            Expr::ArrayFlatMap { array, callback } => {
                (array, "flatMap", std::slice::from_ref(callback))
            }
            Expr::ArrayFilter { array, callback } => {
                (array, "filter", std::slice::from_ref(callback))
            }
            Expr::ArraySome { array, callback } => (array, "some", std::slice::from_ref(callback)),
            Expr::ArrayEvery { array, callback } => {
                (array, "every", std::slice::from_ref(callback))
            }
            Expr::ArrayFind { array, callback } => (array, "find", std::slice::from_ref(callback)),
            Expr::ArrayFindIndex { array, callback } => {
                (array, "findIndex", std::slice::from_ref(callback))
            }
            Expr::ArraySort { array, comparator } => {
                (array, "sort", std::slice::from_ref(comparator))
            }
            Expr::ArrayToSorted {
                array,
                comparator: Some(c),
                ..
            } => (array, "toSorted", std::slice::from_ref(c)),
            Expr::Call { callee, args, .. }
                if let Expr::PropertyGet {
                    object, property, ..
                } = callee.as_ref() =>
            {
                (object, hir_compat::method(property).name(), args.as_slice())
            }
            _ => return Ok(None),
        };
        let raw_obj = self.eval_expr(s, object)?;
        if raw_obj == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let (raw_stripped, _) = self.types.strip_nullish(raw_obj);

        if method_name == "forEach"
            && let TyKind::Generic { base, args: gargs } = self.types.kind(raw_stripped)
            && let HirType::Named(base_name) = base.hir(self.ctx)
            && let Some(cb) = args.first()
        {
            let inputs = match (
                base_name.as_str(),
                self.types.ty_slice(gargs).to_vec().as_slice(),
            ) {
                ("Map" | "ReadonlyMap", [k, v]) => vec![*v, *k, raw_stripped],
                ("Set" | "ReadonlySet", [v]) => vec![*v, *v, raw_stripped],
                _ => return Ok(None),
            };
            return Ok(Some(Ok(CallbackSite::new(
                cb,
                inputs,
                MethodResult::Void,
                raw_obj,
            ))));
        }

        let Some(method) = builtins::receiver_of(self.ctx, self.types, raw_stripped)
            .and_then(|r| Method::lookup(r, method_name))
        else {
            return Ok(None);
        };
        let Some(cb_kind) = method.callback() else {
            return Ok(None);
        };
        let obj_ty = self.resolve_cell_read(s.inst, raw_obj);
        if obj_ty == Ty::BOTTOM {
            return Ok(Some(Err(Ty::BOTTOM)));
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);
        let Some((first_arg, tail)) = args.split_first() else {
            return Ok(None);
        };

        if matches!(cb_kind, Cb::Replacement) {
            let Some(cb) = tail.first() else {
                return Ok(None);
            };
            let cb_ty = self.eval_expr(s, cb)?;
            if cb_ty == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            if !matches!(self.types.kind(cb_ty), TyKind::Func(_)) {
                return Ok(None);
            }
            let mut inputs = vec![Ty::STRING];
            let captures = match first_arg {
                Expr::RegExp { pattern, .. } => Some(crate::methods::regex_captures(pattern)),
                Expr::LocalGet(id) => self
                    .index
                    .local_idx(*id)
                    .and_then(|li| {
                        self.index
                            .local_regex_pattern
                            .get(li.index())
                            .copied()
                            .flatten()
                    })
                    .map(crate::methods::regex_captures),
                Expr::String(_) => Some((0, false)),
                _ => None,
            };
            if let Some((count, named)) = captures {
                let opt = self.types.with_absences(Ty::STRING, Absence::Undefined);
                inputs.extend(std::iter::repeat_n(opt, count));
                inputs.extend([Ty::NUMBER, Ty::STRING]);
                if named {
                    inputs.push(self.types.object(&[], Some(opt)));
                }
            }
            return Ok(Some(Ok(CallbackSite::new(
                cb,
                inputs,
                method.result_rule(),
                obj_ty,
            ))));
        }

        if matches!(cb_kind, Cb::Comparator) && matches!(first_arg, Expr::Undefined) {
            return Ok(None);
        }
        let Some(elem) = builtins::iterated_element(self.ctx, self.types, stripped) else {
            return Ok(None);
        };
        let (inputs, init_ty) = if matches!(cb_kind, Cb::Fold) {
            let acc = match tail.first() {
                Some(init) => self.eval_expr(s, init)?,
                None => elem,
            };
            if acc == Ty::BOTTOM {
                return Ok(Some(Err(Ty::BOTTOM)));
            }
            (vec![acc, elem, Ty::NUMBER, stripped], Some(acc))
        } else {
            (self.array_cb_inputs(cb_kind, stripped, elem), None)
        };
        let mut site = CallbackSite::new(first_arg, inputs, method.result_rule(), obj_ty);
        site.init_ty = init_ty;
        site.sparse_arrays = self.facts.sparse_arrays;
        Ok(Some(Ok(site)))
    }

    pub(super) fn try_eval_builtin_callback(
        &mut self,
        s: &mut EvalState<'h>,
        expr: &'h Expr,
    ) -> Result<Option<Ty>> {
        let Some(matched) = self.match_builtin_callback(s, expr)? else {
            return Ok(None);
        };
        let CallbackSite {
            callback,
            mut inputs,
            rule,
            recv_ty,
            init_ty,
            sparse_arrays,
            group_elem,
        } = match matched {
            Ok(site) => site,
            Err(early) => return Ok(Some(early)),
        };
        if inputs.contains(&Ty::BOTTOM) {
            return Ok(Some(Ty::BOTTOM));
        }
        let ident = if let Expr::Closure { func_id, .. } = callback {
            let Some(template) = self.index.template_of_func(*func_id) else {
                return Ok(None);
            };
            let capture_owner = if self.index.template_captures(template).is_empty() {
                CaptureOwner::None
            } else if self.collapsed_closures.contains(template.index())
                && let Some(owner_tid) = self.instances.get(s.inst).map(|i| i.template)
            {
                CaptureOwner::Template(owner_tid)
            } else {
                CaptureOwner::Instance(s.inst)
            };
            FuncIdentity {
                template,
                capture_owner,
            }
        } else {
            let cb_val = self.eval_expr(s, callback)?;
            if cb_val == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            if let Some(idents) = self.types.func_union_members(cb_val) {
                let mut arms = Vec::with_capacity(idents.len());
                let mut cb_ret = Ty::BOTTOM;
                let mut max_params = 0usize;
                for ident in idents {
                    let Some(cb_view) = self.index.view(ident.template) else {
                        return Ok(None);
                    };
                    let mut arm_inputs = inputs.clone();
                    if !cb_view.params.iter().any(|p| p.is_rest)
                        && arm_inputs.len() > cb_view.params.len()
                    {
                        arm_inputs.truncate(cb_view.params.len());
                    }
                    max_params = max_params.max(arm_inputs.len());
                    let cb_inst = self.request_instance(
                        InstanceReq::new(ident.template, &arm_inputs, Some(s.inst))
                            .owner(ident.capture_owner),
                    )?;
                    if let Some(ret_loc) = self.instances.get(cb_inst).map(|i| i.return_loc) {
                        let r = self.read_loc(ret_loc, s.inst);
                        cb_ret = self.types.join(cb_ret, r);
                    }
                    arms.push((ident, cb_inst));
                }
                inputs.truncate(max_params);
                let arms_span = self.intern_dispatch_arms(&arms);
                let input_tys = self.types.tuple(&inputs);
                let helper = self.intern_helper(
                    HelperSpec::Dispatcher {
                        callee_ty: cb_val,
                        arms: arms_span,
                        arg_tys: input_tys,
                        strip_arg_mask: 0,
                        recv_ty: None,
                    },
                    0,
                );
                self.record_site_coercion(
                    s.inst,
                    callback,
                    FuncCoercion::DispatchCallback {
                        helper,
                        arms: arms_span,
                        input_tys,
                    },
                );
                if let Some(elem) = group_elem {
                    if cb_ret == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let key = if self.types.is_open(cb_ret) {
                        Ty::STRING
                    } else {
                        cb_ret
                    };
                    let arr = self.types.array(elem);
                    return Ok(Some(self.types.generic(
                        shapes::TypeId::from_owned(self.ctx, HirType::Named("Map".into())),
                        &[key, arr],
                    )));
                }
                return Ok(Some(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    rule,
                    MethodResultArgs {
                        receiver: recv_ty,
                        arg0: init_ty,
                        arg0_expr: None,
                        callback_result: Some(cb_ret),
                        sparse_arrays,
                    },
                )));
            }
            let TyKind::Func(ident) = self.types.kind(cb_val) else {
                return Ok(None);
            };
            if self.is_escaping_env(ident.template, s.inst) {
                let Some(cb_view) = self.index.view(ident.template) else {
                    return Ok(None);
                };
                if !cb_view.params.iter().any(|p| p.is_rest) && inputs.len() > cb_view.params.len()
                {
                    inputs.truncate(cb_view.params.len());
                }
                let cb_inst = self.request_instance(
                    InstanceReq::new(ident.template, &inputs, Some(s.inst))
                        .owner(ident.capture_owner),
                )?;
                let input_tys = self.types.tuple(&inputs);
                self.record_site_coercion(
                    s.inst,
                    callback,
                    FuncCoercion::EnvCallback {
                        target: cb_inst,
                        input_tys,
                    },
                );
                let Some(cb_ret_loc) = self.instances.get(cb_inst).map(|i| i.return_loc) else {
                    return Ok(None);
                };
                let cb_ret = self.read_loc(cb_ret_loc, s.inst);
                if let Some(elem) = group_elem {
                    if cb_ret == Ty::BOTTOM {
                        return Ok(Some(Ty::BOTTOM));
                    }
                    let key = if self.types.is_open(cb_ret) {
                        Ty::STRING
                    } else {
                        cb_ret
                    };
                    let arr = self.types.array(elem);
                    return Ok(Some(self.types.generic(
                        shapes::TypeId::from_owned(self.ctx, HirType::Named("Map".into())),
                        &[key, arr],
                    )));
                }
                return Ok(Some(builtins::apply_method_result(
                    self.ctx,
                    self.types,
                    rule,
                    MethodResultArgs {
                        receiver: recv_ty,
                        arg0: init_ty,
                        arg0_expr: None,
                        callback_result: Some(cb_ret),
                        sparse_arrays,
                    },
                )));
            }
            ident
        };
        let Some(cb_view) = self.index.view(ident.template) else {
            return Ok(None);
        };
        let caller_tid = self.instances.get(s.inst).map(|i| i.template);
        let caller_is_env = caller_tid.is_some_and(|t| self.is_escaping_env(t, s.inst));
        let keep_inline = matches!(callback, Expr::Closure { .. })
            && (cb_view.captures_this
                || cb_view.captures_new_target
                || (caller_is_env && !self.index.template_captures(ident.template).is_empty()));
        if !cb_view.params.iter().any(|p| p.is_rest) && inputs.len() > cb_view.params.len() {
            inputs.truncate(cb_view.params.len());
        }
        let cb_inst = self.request_instance(
            InstanceReq::new(ident.template, &inputs, Some(s.inst))
                .receiver(keep_inline.then_some(s.receiver).flatten())
                .owner(ident.capture_owner)
                .inline_cb(),
        )?;
        self.record_site(
            s.inst,
            callback,
            SiteResolution::Callback {
                target: cb_inst,
                keep_inline_closure: keep_inline,
            },
        );
        let Some(cb_ret_loc) = self.instances.get(cb_inst).map(|i| i.return_loc) else {
            return Ok(None);
        };
        let cb_ret = self.read_loc(cb_ret_loc, s.inst);

        if let Some(elem) = group_elem {
            if cb_ret == Ty::BOTTOM {
                return Ok(Some(Ty::BOTTOM));
            }
            let key = if self.types.is_open(cb_ret) {
                Ty::STRING
            } else {
                cb_ret
            };
            let arr = self.types.array(elem);
            return Ok(Some(self.types.generic(
                shapes::TypeId::from_owned(self.ctx, HirType::Named("Map".into())),
                &[key, arr],
            )));
        }
        Ok(Some(builtins::apply_method_result(
            self.ctx,
            self.types,
            rule,
            MethodResultArgs {
                receiver: recv_ty,
                arg0: init_ty,
                arg0_expr: None,
                callback_result: Some(cb_ret),
                sparse_arrays,
            },
        )))
    }

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
        let obj_ty = self.types.object(&props, None);
        if has_zero_sized_func {
            self.record_site(s.inst, site_expr, SiteResolution::ObjectLiteral(obj_ty));
        }
        Ok(obj_ty)
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
            .and_then(|cid| self.index.class(cid))
            .and_then(|c| ClassInfo::find(&c.static_methods, mn))
        {
            let mask = self.callable_arg_mask_in(s.inst, &arg_tys);
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(tid, &arg_tys, Some(s.inst)).at(byte_offset),
                true,
                mask,
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
        req: InstanceReq<'_>,
        is_method: bool,
        strip_arg_mask: u64,
        pass_env: bool,
    ) -> Result<(InstanceId, Ty)> {
        let pass_receiver = matches!(req.receiver, Some(Receiver::Object(_)));
        let target = self.request_instance(req)?;
        let res = if is_method {
            SiteResolution::MethodCall {
                target,
                strip_arg_mask,
            }
        } else {
            SiteResolution::DirectCall {
                target,
                strip_arg_mask,
                pass_receiver,
                pass_env,
            }
        };
        self.record_site(inst_id, expr, res);
        let return_loc = self
            .instances
            .get(target)
            .ok_or_else(|| Diagnostic::internal("missing call target instance"))?
            .return_loc;
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
        let arg_tys = self.eval_args(s, args)?;
        if let Some(cid) = self.index.class_by_name_id(cname_id) {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            let class_args = if !type_args.is_empty() {
                self.eval_type_args(s.inst, type_args)
            } else if let Some(tps) = self
                .class_type_params
                .get(cid.index())
                .filter(|t| !t.is_empty())
                .cloned()
            {
                let mut bindings = Vec::new();
                if let Some(ctor_view) = self
                    .index
                    .class(cid)
                    .and_then(|c| c.ctor)
                    .and_then(|t| self.index.view(t))
                {
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
            let strip_arg_mask = self.callable_arg_mask_in(s.inst, &arg_tys);
            self.record_site(
                s.inst,
                expr,
                SiteResolution::ClassNew {
                    class_inst,
                    strip_arg_mask,
                },
            );
            return Ok(self.types.class_inst(class_inst));
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
            let arg_tys = self.eval_args(s, args)?;
            if let Some(res) = self.eval_builtin_import_call(name, args, &arg_tys) {
                return Ok(res);
            }
            let is_imported = self.hir.imports.iter().any(|imp| {
                imp.specifiers.iter().any(|sp| match sp {
                    perry_hir::ir::ImportSpecifier::Named { local, .. }
                    | perry_hir::ir::ImportSpecifier::Default { local }
                    | perry_hir::ir::ImportSpecifier::Namespace { local } => local == name,
                })
            });
            if byte_offset != 0
                && !is_imported
                && !name.starts_with("js_")
                && !name.starts_with("__")
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
        let Some(target_view) = self.index.view(ident.template) else {
            return Ok(Ty::OPEN);
        };

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
        if !target_view.params.iter().any(|p| p.is_rest) && arg_tys.len() > target_view.params.len()
        {
            self.check_escaping_unannotated_func(ident.template, None)?;
        }
        self.check_callback_params(s.inst, target_view, &arg_tys, byte_offset)?;

        let explicit_targs = self.eval_type_args(s.inst, type_args);
        let strip_mask = self.callable_arg_mask_in(s.inst, &arg_tys);
        let pass_env = self.is_escaping_env(ident.template, s.inst);
        let (target_inst, ret) = self.dispatch_call_site(
            s.inst,
            expr,
            InstanceReq::new(ident.template, &arg_tys, Some(s.inst))
                .owner(ident.capture_owner)
                .type_args(&explicit_targs)
                .at(byte_offset),
            false,
            strip_mask,
            pass_env,
        )?;
        if let Some(tw) = self.inst_written.get(target_inst.index()).cloned()
            && let Some(cw) = self.inst_written.get_mut(s.inst.index())
        {
            cw.union_with(&tw);
        }
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
            0,
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

        if matches!(object, Expr::GlobalGet(_)) && name == "fetch" {
            self.eval_args(s, args)?;
            let resp = self.types.named(self.ctx, "Response");
            return Ok(self.types.promise(resp));
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
            let arg_tys = self.eval_args(s, args)?;
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
        let arg_tys = self.eval_args(s, args)?;
        if obj_ty == Ty::BOTTOM {
            return Ok(Ty::BOTTOM);
        }
        let (stripped, _) = self.types.strip_nullish(obj_ty);

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
            && let Some(cinst) = self.class_insts.get(cinst_id)
            && let Some(method_tid) = self
                .index
                .class(cinst.class)
                .and_then(|c| ClassInfo::find(&c.methods, mname_id))
        {
            if arg_tys.contains(&Ty::BOTTOM) {
                return Ok(Ty::BOTTOM);
            }
            if let Some(view) = self.index.view(method_tid) {
                self.check_callback_params(s.inst, view, &arg_tys, byte_offset)?;
            }
            let mask = self.callable_arg_mask_in(s.inst, &arg_tys);
            let (_, ret) = self.dispatch_call_site(
                s.inst,
                expr,
                InstanceReq::new(method_tid, &arg_tys, Some(s.inst))
                    .receiver(Some(Receiver::Class(cinst_id)))
                    .at(byte_offset),
                true,
                mask,
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
            let Some(method_view) = self.index.view(ident.template) else {
                return Ok(Ty::OPEN);
            };
            self.check_callback_params(s.inst, method_view, &arg_tys, byte_offset)?;
            let mask = self.callable_arg_mask_in(s.inst, &arg_tys);
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
                mask,
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
        let is_collection_or_open = self.types.is_open(stripped)
            || matches!(
                self.types.kind(stripped),
                TyKind::Array(_) | TyKind::Cell { .. }
            )
            || matches!(self.types.kind(stripped), TyKind::Generic { base, .. }
                if matches!(base.hir(self.ctx), HirType::Named(n) if matches!(n.as_str(), "Map" | "Set" | "ReadonlyMap" | "ReadonlySet")));
        if byte_offset != 0
            && !is_collection_or_open
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
        let strip_arg_mask = self.callable_arg_mask_in(s.inst, arg_tys);
        let mut arms = Vec::with_capacity(idents.len());
        let mut ret = Ty::BOTTOM;
        for &ident in idents {
            let Some(view) = self.index.view(ident.template) else {
                return Ok(Ty::OPEN);
            };
            self.check_callback_params(s.inst, view, arg_tys, byte_offset)?;
            let recv = recv_ty.filter(|_| view.uses_this).map(Receiver::Object);
            let target = self.request_instance(
                InstanceReq::new(ident.template, arg_tys, Some(s.inst))
                    .receiver(recv)
                    .owner(ident.capture_owner)
                    .at(byte_offset),
            )?;
            if let Some(tw) = self.inst_written.get(target.index()).cloned()
                && let Some(cw) = self.inst_written.get_mut(s.inst.index())
            {
                cw.union_with(&tw);
            }
            let return_loc = self
                .instances
                .get(target)
                .ok_or_else(|| Diagnostic::internal("missing dispatch call target instance"))?
                .return_loc;
            let r = self.read_loc(return_loc, s.inst);
            ret = self.types.join(ret, r);
            arms.push((ident, target));
        }
        let arms_span = self.intern_dispatch_arms(&arms);
        let arg_tuple = self.types.tuple(arg_tys);
        let needs_recv = recv_ty.is_some()
            && arms
                .iter()
                .any(|&(ident, _)| self.index.view(ident.template).is_some_and(|v| v.uses_this));
        let helper = self.intern_helper(
            HelperSpec::Dispatcher {
                callee_ty,
                arms: arms_span,
                arg_tys: arg_tuple,
                strip_arg_mask,
                recv_ty: needs_recv.then_some(recv_ty).flatten(),
            },
            byte_offset,
        );
        self.record_site(
            s.inst,
            expr,
            SiteResolution::DispatchCall {
                helper,
                strip_arg_mask,
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
            let Some(cb_view) = self.index.view(ident.template) else {
                continue;
            };
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

    pub(super) fn eval_builtin_import_call(
        &mut self,
        local_name: &str,
        args: &[Expr],
        arg_tys: &[Ty],
    ) -> Option<Ty> {
        let (source, imported) = self.hir.imports.iter().find_map(|imp| {
            imp.specifiers.iter().find_map(|s| match s {
                perry_hir::ir::ImportSpecifier::Named { imported, local }
                    if local == local_name =>
                {
                    Some((imp.source.as_str(), imported.as_str()))
                }
                _ => None,
            })
        })?;
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
        let Some(view) = self.index.view(template) else {
            return Ok(());
        };
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
