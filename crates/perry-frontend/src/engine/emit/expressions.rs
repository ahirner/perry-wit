//! Emit expressions from solved call sites and function-value coercions.
use super::*;
use crate::hir_compat::{self, GuestCall};

enum CalleeValue {
    Static,
    Environment,
    Tagged,
}

impl<'h> Emitter<'h, '_> {
    pub(super) fn rewrite_expr(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        let site_entry = self.site_entry(rctx.inst_id, expr).cloned();
        let coercion = site_entry.as_ref().and_then(|e| e.coercion);
        let resolution = site_entry.and_then(|e| e.resolution);
        if let Some(c) = coercion {
            match c {
                FuncCoercion::Zero if is_pure_func_expr(expr) => return Ok(Expr::Number(0.0)),
                FuncCoercion::Tag(ident) if is_pure_func_expr(expr) => {
                    return Ok(Self::emit_tag_record(ident));
                }
                _ => {}
            }
        }
        let rewritten = self.rewrite_expr_inner(rctx, expr, resolution)?;
        match coercion {
            Some(FuncCoercion::Zero) => Ok(Expr::Sequence(vec![rewritten, Expr::Number(0.0)])),
            Some(FuncCoercion::Tag(ident)) => {
                let tag = Self::emit_tag_record(ident);
                Ok(Expr::Sequence(vec![rewritten, tag]))
            }
            Some(FuncCoercion::BoxEnv(ident)) => {
                let val_expr = Self::prop_get(rewritten, "val");
                Ok(Self::emit_env_wrapper(ident, val_expr))
            }
            Some(FuncCoercion::Widen(helper)) => {
                let fid = self.emit_helper(helper)?;
                Ok(Self::synth_call(fid, vec![rewritten], 0, 0))
            }
            Some(FuncCoercion::EnvCallback { target, input_tys }) => {
                let target_fid = self
                    .instance(target, "missing env callback target instance")?
                    .emitted_func_id;
                let inputs = self.types.tuple_slice(input_tys);
                let arm_ret = self.arm_return_hir(target)?;
                self.emit_callback_wrapper(
                    target_fid,
                    Self::prop_get(rewritten, "val"),
                    inputs,
                    arm_ret,
                )
            }
            Some(FuncCoercion::DispatchCallback {
                helper,
                arms,
                input_tys,
            }) => {
                let arm_slice = self.dispatch_arms_slice(arms)?;
                let inputs = self.types.tuple_slice(input_tys);
                let max_params = arm_slice
                    .iter()
                    .filter_map(|&(ident, _)| self.index.view(ident.template))
                    .map(|v| {
                        if v.params.iter().any(|p| p.is_rest) {
                            inputs.len()
                        } else {
                            v.params.len()
                        }
                    })
                    .max()
                    .unwrap_or(0)
                    .min(inputs.len());
                let disp_fid = self.emit_helper(helper)?;
                let disp_ret = self
                    .synth_functions
                    .iter()
                    .find(|f| f.id == disp_fid)
                    .ok_or_else(|| Diagnostic::internal("missing synthesized dispatcher"))?
                    .return_type
                    .clone();
                self.emit_callback_wrapper(disp_fid, rewritten, &inputs[..max_params], disp_ret)
            }
            None => Ok(rewritten),
        }
    }

    fn rewrite_expr_inner(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: &'h Expr,
        resolution: Option<SiteResolution>,
    ) -> Result<Expr> {
        if let Some(site) = resolution {
            match site {
                SiteResolution::BranchResult(ty) => {
                    let value = self.rewrite_expr_inner(&rctx.with_expected(true), expr, None)?;
                    let ty = self.materialize(ty)?;
                    return Ok(GuestCall::Typed { value, ty }.into_hir());
                }
                SiteResolution::Cell(_) => {}
                SiteResolution::FoldBool(b) => {
                    if let Expr::Compare { left, right, .. } = expr {
                        return self.fold_comparison(rctx, left, right, b);
                    }
                }
                SiteResolution::CollectionNew { is_map, type_args } => {
                    let mat_args = type_args
                        .into_iter()
                        .map(|t| self.materialize(t))
                        .collect::<Result<Vec<_>>>()?;
                    return Ok(hir_compat::typed_collection(is_map, mat_args));
                }
                SiteResolution::ThrowTypeError(helper) => {
                    let fid = self.emit_helper(helper)?;
                    let call = Self::synth_call(fid, Vec::new(), 0, 0);
                    if let Expr::Call { callee, args, .. } = expr {
                        let mut effects = Vec::new();
                        if !is_pure_func_expr(callee) {
                            effects.push(self.rewrite_expr(rctx, callee)?);
                        }
                        for a in args {
                            if !is_pure_func_expr(a) {
                                effects.push(self.rewrite_expr(rctx, a)?);
                            }
                        }
                        return Ok(if effects.is_empty() {
                            call
                        } else {
                            effects.push(call);
                            Expr::Sequence(effects)
                        });
                    }
                    return Ok(call);
                }
                SiteResolution::FuncTypeOf(helper) => {
                    if let Expr::TypeOf(operand) = expr {
                        let Some(helper) = helper else {
                            let s = Expr::String("function".into());
                            return Ok(if is_pure_func_expr(operand) {
                                s
                            } else {
                                Expr::Sequence(vec![self.rewrite_expr(rctx, operand)?, s])
                            });
                        };
                        let fid = self.emit_helper(helper)?;
                        let arg = self.rewrite_expr(rctx, operand)?;
                        return Ok(Self::synth_call(fid, vec![arg], 0, 0));
                    }
                }
                SiteResolution::Callback {
                    target,
                    keep_inline_closure,
                } => {
                    let (emitted_func_id, return_loc, param_locs_span) = {
                        let ti = self.instance(target, "missing callback target instance")?;
                        (ti.emitted_func_id, ti.return_loc, ti.param_locs)
                    };
                    if !keep_inline_closure {
                        return Ok(Expr::FuncRef(emitted_func_id));
                    }
                    if let Expr::Closure { body, .. } = expr {
                        let mut out = expr.clone();
                        if let Expr::Closure {
                            params,
                            return_type,
                            body: out_body,
                            captures,
                            mutable_captures,
                            ..
                        } = &mut out
                        {
                            *return_type = self.materialize(self.solved.locations[return_loc])?;
                            let param_tys: Vec<Ty> = self
                                .solved
                                .param_locs
                                .get(param_locs_span.range())
                                .unwrap_or(&[])
                                .iter()
                                .map(|&loc| self.solved.locations[loc])
                                .collect();
                            for (p, ty) in params.iter_mut().zip(param_tys) {
                                if self.is_resolved(ty) {
                                    p.ty = self.materialize(ty)?;
                                }
                            }
                            let mut cb_ctx = RewriteCtx {
                                inst_id: target,
                                func_params: &[],
                                self_local: None,
                                env_local: rctx.env_local,
                                env_captures: rctx.env_captures,
                                boxed_param_map: rctx.boxed_param_map,
                                void_return: false,
                                has_expected: false,
                                remap: None,
                            };
                            *out_body = self.rewrite_stmts(&mut cb_ctx, body)?;
                            if rctx.env_local.is_some()
                                || !rctx.boxed_param_map.is_empty()
                                || captures.iter().any(|c| self.is_boxed_local(*c))
                            {
                                let mut new_caps = Vec::new();
                                let mut defined: std::collections::HashSet<usize> =
                                    params.iter().map(|p| p.id as usize).collect();
                                for s in out_body.iter() {
                                    perry_hir::analysis::collect_local_refs_stmt(
                                        s,
                                        &mut new_caps,
                                        &mut defined,
                                    );
                                }
                                *captures = new_caps;
                                mutable_captures.retain(|c| {
                                    !self.is_boxed_local(*c)
                                        && !rctx.env_captures.iter().any(|(lid, _, _)| lid == c)
                                        && !rctx.boxed_param_map.iter().any(|(lid, _)| lid == c)
                                });
                            }
                        }
                        return Ok(out);
                    }
                }
                SiteResolution::DirectCall {
                    target,
                    argument_types,
                    pass_receiver,
                    pass_env,
                } => {
                    let fid = self
                        .instance(target, "missing direct call target instance")?
                        .emitted_func_id;
                    if let Expr::CallSpread {
                        args,
                        type_args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                    {
                        let args = args
                            .iter()
                            .map(|arg| match arg {
                                CallArg::Expr(e) => self.rewrite_expr(rctx, e).map(CallArg::Expr),
                                CallArg::Spread(e) => {
                                    self.rewrite_expr(rctx, e).map(CallArg::Spread)
                                }
                            })
                            .collect::<Result<_>>()?;
                        return Ok(Expr::CallSpread {
                            callee: Box::new(Expr::FuncRef(fid)),
                            args,
                            type_args: type_args.clone(),
                            byte_offset: *byte_offset,
                            byte_end: *byte_end,
                        });
                    }
                    return self.rewrite_call(
                        rctx,
                        expr,
                        fid,
                        argument_types,
                        pass_receiver,
                        if pass_env {
                            CalleeValue::Environment
                        } else {
                            CalleeValue::Static
                        },
                    );
                }
                SiteResolution::FuncCompare {
                    left_ty,
                    right_ty,
                    op,
                    helper,
                } => {
                    if let Expr::Compare { left, right, .. } = expr {
                        let is_eq = matches!(op, CompareOp::Eq | CompareOp::LooseEq);
                        let l_zero = self.is_zero_sized_func(left_ty);
                        let r_zero = self.is_zero_sized_func(right_ty);
                        if l_zero || r_zero {
                            let (zero_ty, union_expr, union_ty) = if l_zero {
                                (left_ty, right.as_ref(), right_ty)
                            } else {
                                (right_ty, left.as_ref(), left_ty)
                            };
                            if let TyKind::Func(ident) = self.types.kind(zero_ty) {
                                if !self
                                    .types
                                    .func_members(&union_ty)
                                    .any(|member| member == ident)
                                {
                                    return self.fold_comparison(rctx, left, right, !is_eq);
                                }
                                let u_rewritten = self.rewrite_expr(rctx, union_expr)?;
                                let comparison = Expr::Compare {
                                    op: if is_eq { CompareOp::Eq } else { CompareOp::Ne },
                                    left: Box::new(Self::prop_get(u_rewritten, "tag")),
                                    right: Box::new(Expr::String(func_tag(ident))),
                                };
                                let erased = if l_zero { left } else { right };
                                if is_pure_func_expr(erased) {
                                    return Ok(comparison);
                                }
                                let effects = vec![self.rewrite_expr(rctx, erased)?];
                                return if l_zero {
                                    Ok(sequence(effects, comparison))
                                } else {
                                    self.followed_by(comparison, effects)
                                };
                            }
                        }
                        let l_env = self
                            .types
                            .is_env_func(left_ty, &self.solved.escaping_env_templates);
                        let r_env = self
                            .types
                            .is_env_func(right_ty, &self.solved.escaping_env_templates);
                        let l_rewritten = self.rewrite_expr(rctx, left)?;
                        let r_rewritten = self.rewrite_expr(rctx, right)?;
                        if l_env && r_env && left_ty == right_ty {
                            return Ok(Expr::Compare {
                                op: if is_eq { CompareOp::Eq } else { CompareOp::Ne },
                                left: Box::new(Self::prop_get(l_rewritten, "val")),
                                right: Box::new(Self::prop_get(r_rewritten, "val")),
                            });
                        }
                        let helper_id = helper.ok_or_else(|| {
                            Diagnostic::internal("missing synthesized comparer helper")
                        })?;
                        let cmp_fid = self.emit_helper(helper_id)?;
                        let call = Self::synth_call(cmp_fid, vec![l_rewritten, r_rewritten], 0, 0);
                        return Ok(if is_eq {
                            call
                        } else {
                            Expr::Unary {
                                op: UnaryOp::Not,
                                operand: Box::new(call),
                            }
                        });
                    }
                }
                SiteResolution::ObjectLiteral(obj_ty) => {
                    let pairs: Vec<(&str, &'h Expr)> = match expr {
                        Expr::Object(fields) => {
                            fields.iter().map(|(k, v)| (k.as_str(), v)).collect()
                        }
                        Expr::New {
                            class_name, args, ..
                        } => self
                            .hir
                            .classes
                            .iter()
                            .find(|c| c.name == *class_name)
                            .ok_or_else(|| {
                                Diagnostic::internal(format!(
                                    "missing object literal class '{class_name}'"
                                ))
                            })?
                            .fields
                            .iter()
                            .zip(args)
                            .map(|(f, a)| (f.name.as_str(), a))
                            .collect(),
                        _ => Vec::new(),
                    };
                    let props = match self.types.kind(obj_ty) {
                        TyKind::Object { props, .. } => self.types.object_slice(props),
                        _ => {
                            return Err(Diagnostic::internal(
                                "object literal site resolved to non-object type",
                            ));
                        }
                    };
                    let mut kept_names = Vec::new();
                    let mut kept_args = Vec::new();
                    let mut effects = Vec::new();
                    for ((k, v), prop) in pairs.into_iter().zip(props) {
                        if !self.is_zero_sized_func(prop.ty) {
                            kept_names.push(k);
                            kept_args.push(sequence(
                                std::mem::take(&mut effects),
                                self.rewrite_expr(rctx, v)?,
                            ));
                        } else if !is_pure_func_expr(v) {
                            effects.push(self.rewrite_expr(rctx, v)?);
                        }
                    }
                    let effects = self.finish_operands(&mut kept_args, effects)?;
                    return Ok(sequence(
                        effects,
                        Expr::Object(
                            kept_names
                                .into_iter()
                                .map(str::to_owned)
                                .zip(kept_args)
                                .collect(),
                        ),
                    ));
                }
                SiteResolution::DispatchCall {
                    helper,
                    argument_types,
                    needs_recv,
                } => {
                    let fid = self.emit_helper(helper)?;
                    return self.rewrite_call(
                        rctx,
                        expr,
                        fid,
                        argument_types,
                        needs_recv,
                        CalleeValue::Tagged,
                    );
                }
                SiteResolution::TypedArray(ty) => {
                    use perry_hir::ir::ArrayElement;
                    let expected = rctx.with_expected(true);
                    let array = match expr {
                        Expr::Array(items, _) => Expr::Array(
                            items
                                .iter()
                                .map(|item| self.rewrite_expr(&expected, item))
                                .collect::<Result<_>>()?,
                            None,
                        ),
                        Expr::ArraySpread(items) => {
                            let untyped = rctx.with_expected(false);
                            let items = items
                                .iter()
                                .map(|item| {
                                    Ok(match item {
                                        ArrayElement::Hole => ArrayElement::Hole,
                                        ArrayElement::Expr(e) => {
                                            ArrayElement::Expr(self.rewrite_expr(&expected, e)?)
                                        }
                                        ArrayElement::Spread(e) => {
                                            ArrayElement::Spread(self.rewrite_expr(&untyped, e)?)
                                        }
                                    })
                                })
                                .collect::<Result<_>>()?;
                            Expr::ArraySpread(items)
                        }
                        _ => {
                            return Err(Diagnostic::internal(
                                "array site requires an array literal",
                            ));
                        }
                    };
                    if rctx.has_expected {
                        return Ok(array);
                    }
                    let ty = self.materialize(ty)?;
                    return self.inline_iife(ty, vec![Stmt::Return(Some(array))]);
                }
                SiteResolution::ZeroSizedArray => {
                    if let Expr::Array(elems, _) = expr {
                        let mut out_elems = Vec::with_capacity(elems.len());
                        for e in elems {
                            if is_pure_func_expr(e) {
                                out_elems.push(Expr::Number(0.0));
                            } else {
                                out_elems.push(Expr::Sequence(vec![
                                    self.rewrite_expr(rctx, e)?,
                                    Expr::Number(0.0),
                                ]));
                            }
                        }
                        return Ok(Expr::Array(out_elems, None));
                    }
                }
                SiteResolution::ZeroSizedFieldSet => {
                    if let Expr::PropertySet { object, value, .. }
                    | Expr::PutValueSet {
                        target: object,
                        value,
                        ..
                    } = expr
                    {
                        let mut effects = Vec::new();
                        if !is_pure_func_expr(object) {
                            effects.push(self.rewrite_expr(rctx, object)?);
                        }
                        if !is_pure_func_expr(value) {
                            effects.push(self.rewrite_expr(rctx, value)?);
                        }
                        return Ok(if effects.is_empty() {
                            Expr::Undefined
                        } else {
                            effects.push(Expr::Undefined);
                            Expr::Sequence(effects)
                        });
                    }
                }
                SiteResolution::MethodCall {
                    target,
                    argument_types,
                } => {
                    let method_name = self
                        .instance(target, "missing method call target instance")?
                        .emitted_method_name
                        .clone();
                    let mut out = expr.clone();
                    match (expr, &mut out) {
                        (
                            Expr::StaticMethodCall { args, .. },
                            Expr::StaticMethodCall {
                                method_name: out_name,
                                args: out_args,
                                ..
                            },
                        ) => {
                            *out_name = method_name;
                            out_args.clear();
                            let before =
                                self.rewrite_filtered_args(rctx, args, argument_types, out_args)?;
                            return Ok(sequence(before, out));
                        }
                        (
                            Expr::Call { callee, args, .. },
                            Expr::Call {
                                callee: out_callee,
                                args: out_args,
                                type_args,
                                ..
                            },
                        ) if matches!(callee.as_ref(), Expr::PropertyGet { .. }) => {
                            if let (
                                Expr::PropertyGet { object, .. },
                                Expr::PropertyGet {
                                    object: out_obj,
                                    property,
                                    ..
                                },
                            ) = (callee.as_ref(), out_callee.as_mut())
                            {
                                out_args.clear();
                                let effects = self.rewrite_filtered_args(
                                    rctx,
                                    args,
                                    argument_types,
                                    out_args,
                                )?;
                                let receiver = self.rewrite_expr(rctx, object)?;
                                **out_obj = self.followed_by(receiver, effects)?;
                                *property = method_name;
                                type_args.clear();
                                return Ok(out);
                            }
                        }
                        _ => {}
                    }
                }
                SiteResolution::ClassNew {
                    class_inst,
                    argument_types,
                } => {
                    if let Expr::New { args, .. } = expr {
                        let name = self
                            .class_inst_name(class_inst)
                            .ok_or_else(|| Diagnostic::internal("missing class instance name"))?;
                        let mut out = expr.clone();
                        if let Expr::New {
                            class_name,
                            type_args,
                            args: out_args,
                            ..
                        } = &mut out
                        {
                            *class_name = name;
                            type_args.clear();
                            out_args.clear();
                            let before =
                                self.rewrite_filtered_args(rctx, args, argument_types, out_args)?;
                            return Ok(sequence(before, out));
                        }
                    }
                }
                SiteResolution::PromiseNew {
                    value_loc,
                    resolve_tid,
                    reject_tid,
                    executor,
                } => {
                    let (executor, pass_resolve, pass_reject) = executor
                        .ok_or_else(|| Diagnostic::internal("unresolved promise executor"))?;
                    if let Expr::New { args, .. } = expr {
                        return self.emit_promise_new(
                            rctx,
                            args,
                            value_loc,
                            resolve_tid,
                            reject_tid,
                            executor,
                            pass_resolve,
                            pass_reject,
                        );
                    }
                }
                SiteResolution::PromiseReaction {
                    helper,
                    pass_first,
                    pass_second,
                    ..
                } => {
                    if let Expr::Call {
                        callee,
                        args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                        && let Expr::PropertyGet { object, .. } = callee.as_ref()
                    {
                        let fid = self.emit_helper(helper)?;
                        let arg_ctx = rctx.with_expected(true);
                        let mut call_args = vec![self.rewrite_expr(&arg_ctx, object)?];
                        let effects =
                            self.project_arguments(&arg_ctx, args, &mut call_args, |_, i| {
                                matches!(i, 0 if pass_first) || matches!(i, 1 if pass_second)
                            })?;
                        return Ok(sequence(
                            effects,
                            Self::synth_call(fid, call_args, *byte_offset, *byte_end),
                        ));
                    }
                }
                SiteResolution::TimerStart {
                    helper,
                    has_delay,
                    is_immediate,
                    pass_cb,
                } => {
                    if let Expr::Call {
                        args,
                        byte_offset,
                        byte_end,
                        ..
                    } = expr
                    {
                        return self.emit_timer_start(
                            rctx,
                            args,
                            helper,
                            has_delay,
                            is_immediate,
                            pass_cb,
                            *byte_offset,
                            *byte_end,
                        );
                    }
                }
                SiteResolution::TimerClear { arg_ty } => {
                    if let Expr::Call { args, .. } = expr {
                        return self.emit_timer_clear(rctx, args, arg_ty);
                    }
                }
            }
        }
        self.rewrite_expr_plain(rctx, expr)
    }

    fn local_read_override(&self, rctx: &RewriteCtx<'_>, id: LocalId) -> Option<Expr> {
        let undef_box = self.is_undef_boxed(id);
        let unwrap_box = |base: Expr| {
            let val = Self::prop_get(base, "v");
            if undef_box {
                hir_compat::mark_non_null(val)
            } else {
                val
            }
        };
        if let Some(env_pid) = rctx.env_local
            && let Some((_, field, boxed)) = rctx.env_captures.iter().find(|(lid, _, _)| *lid == id)
        {
            let base = Self::prop_get(Expr::LocalGet(env_pid), field);
            return Some(if *boxed { unwrap_box(base) } else { base });
        }
        if let Some(&(_, box_id)) = rctx.boxed_param_map.iter().find(|(lid, _)| *lid == id) {
            return Some(unwrap_box(Expr::LocalGet(box_id)));
        }
        if self.is_boxed_local(id) {
            return Some(unwrap_box(Expr::LocalGet(id)));
        }
        None
    }

    pub(super) fn method_call_expr(recv: Expr, property: &str, args: Vec<Expr>) -> Expr {
        Expr::Call {
            callee: Box::new(Self::prop_get(recv, property)),
            args,
            type_args: Vec::new(),
            byte_offset: 0,
            byte_end: 0,
        }
    }

    fn boxed_target(&self, rctx: &RewriteCtx<'_>, id: LocalId) -> Option<Expr> {
        if let Some(env_pid) = rctx.env_local
            && let Some((_, field, boxed)) = rctx.env_captures.iter().find(|(lid, _, _)| *lid == id)
        {
            return boxed.then(|| Self::prop_get(Expr::LocalGet(env_pid), field));
        }
        if let Some(&(_, box_id)) = rctx.boxed_param_map.iter().find(|(lid, _)| *lid == id) {
            return Some(Expr::LocalGet(box_id));
        }
        if self.is_boxed_local(id) {
            return Some(Expr::LocalGet(id));
        }
        None
    }

    fn rewrite_expr_plain(&mut self, rctx: &RewriteCtx<'_>, expr: &'h Expr) -> Result<Expr> {
        if let Expr::This = expr
            && let Some(sid) = rctx.self_local
        {
            return Ok(Expr::LocalGet(sid));
        }
        if let Expr::Closure { func_id, .. } = expr
            && let Some(tid) = self.index.template_of_func(*func_id)
            && self.is_env_template(tid)
        {
            let capture_owner = if self.index.template_captures(tid).is_empty() {
                CaptureOwner::None
            } else if self.solved.collapsed_closures.contains(tid.index())
                && let Some(owner_tid) = self.solved.instances.get(rctx.inst_id).map(|i| i.template)
            {
                CaptureOwner::Template(owner_tid)
            } else {
                CaptureOwner::Instance(rctx.inst_id)
            };
            let ident = FuncIdentity {
                template: tid,
                capture_owner,
            };
            let class_name = self.ensure_env_class(ident)?;
            let fields = self.env_layout(ident)?;
            let args = if fields.is_empty() {
                vec![Expr::Number(0.0)]
            } else {
                let mut out_args = Vec::with_capacity(fields.len());
                for (cap_id, _, dst_ty, boxed) in fields {
                    let mut arg = self.emit_capture_ref(rctx, cap_id);
                    if !boxed
                        && matches!(capture_owner, CaptureOwner::Template(_))
                        && let Some(src_ty) =
                            self.local_ty_in_owner(CaptureOwner::Instance(rctx.inst_id), cap_id)
                        && src_ty != dst_ty
                    {
                        arg = self.coerce_capture_expr(arg, src_ty, dst_ty)?;
                    }
                    out_args.push(arg);
                }
                out_args
            };
            let env_expr = Expr::New {
                class_name,
                args,
                type_args: Vec::new(),
                byte_offset: 0,
                byte_end: 0,
                cap_args_appended: 0,
            };
            return Ok(Self::emit_env_wrapper(ident, env_expr));
        }
        if let Expr::LocalGet(id) = expr {
            if let Some((_, fid)) = rctx.func_params.iter().find(|(lid, _)| lid == id) {
                return Ok(Expr::FuncRef(*fid));
            }
            if let Some(override_expr) = self.local_read_override(rctx, *id) {
                return Ok(override_expr);
            }
        }
        match expr {
            Expr::ArrayPush {
                array_id, value, ..
            } if let Some(recv) = self.local_read_override(rctx, *array_id) => {
                let arg_ctx = rctx.with_expected(true);
                let val = self.rewrite_expr(&arg_ctx, value)?;
                return Ok(Self::method_call_expr(recv, "push", vec![val]));
            }
            Expr::ArrayPushSpread { array_id, source }
                if let Some(recv) = self.local_read_override(rctx, *array_id) =>
            {
                let src = self.rewrite_expr(rctx, source)?;
                return Ok(Expr::CallSpread {
                    callee: Box::new(Self::prop_get(recv, "push")),
                    args: vec![CallArg::Spread(src)],
                    type_args: Vec::new(),
                    byte_offset: 0,
                    byte_end: 0,
                });
            }
            Expr::ArrayPop(array_id)
                if let Some(recv) = self.local_read_override(rctx, *array_id) =>
            {
                return Ok(Self::method_call_expr(recv, "pop", Vec::new()));
            }
            Expr::ArrayShift(array_id)
                if let Some(recv) = self.local_read_override(rctx, *array_id) =>
            {
                return Ok(Self::method_call_expr(recv, "shift", Vec::new()));
            }
            Expr::ArrayUnshift { array_id, value }
                if let Some(recv) = self.local_read_override(rctx, *array_id) =>
            {
                let arg_ctx = rctx.with_expected(true);
                let val = self.rewrite_expr(&arg_ctx, value)?;
                return Ok(Self::method_call_expr(recv, "unshift", vec![val]));
            }
            Expr::ArraySplice {
                array_id,
                start,
                delete_count,
                items,
            } if let Some(recv) = self.local_read_override(rctx, *array_id) => {
                let mut args = vec![self.rewrite_expr(rctx, start)?];
                if let Some(dc) = delete_count {
                    args.push(self.rewrite_expr(rctx, dc)?);
                }
                for item in items {
                    args.push(self.rewrite_expr(rctx, item)?);
                }
                return Ok(Self::method_call_expr(recv, "splice", args));
            }
            Expr::ArrayCopyWithin {
                array_id,
                target,
                start,
                end,
            } if let Some(recv) = self.local_read_override(rctx, *array_id) => {
                return Ok(Expr::ArrayCopyWithinValue {
                    receiver: Box::new(recv),
                    target: Box::new(self.rewrite_expr(rctx, target)?),
                    start: Box::new(self.rewrite_expr(rctx, start)?),
                    end: self.rewrite_opt_expr(rctx, end.as_deref())?.map(Box::new),
                });
            }
            Expr::SetAdd { set_id, value }
                if let Some(recv) = self.local_read_override(rctx, *set_id) =>
            {
                let val = self.rewrite_expr(rctx, value)?;
                return Ok(Self::method_call_expr(recv, "add", vec![val]));
            }
            _ => {}
        }
        if let Expr::LocalSet(id, value) = expr
            && let Some(box_obj) = self.boxed_target(rctx, *id)
        {
            let val = self.rewrite_expr(rctx, value)?;
            return Ok(Expr::PropertySet {
                object: Box::new(box_obj),
                property: "v".into(),
                value: Box::new(val),
                byte_offset: 0,
                byte_end: 0,
            });
        }
        if let Expr::Conditional {
            condition,
            then_expr,
            else_expr,
        } = expr
        {
            let cond_ctx = rctx.with_expected(false);
            return Ok(Expr::Conditional {
                condition: Box::new(self.rewrite_condition(&cond_ctx, condition)?),
                then_expr: Box::new(self.rewrite_expr(rctx, then_expr)?),
                else_expr: Box::new(self.rewrite_expr(rctx, else_expr)?),
            });
        }
        if let Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } = expr
        {
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                operand: Box::new(self.rewrite_condition(rctx, operand)?),
            });
        }
        if let Expr::Update { id, op, prefix } = expr
            && let Some(box_obj) = self.boxed_target(rctx, *id)
        {
            let bin_op = match op {
                UpdateOp::Increment => BinaryOp::Add,
                UpdateOp::Decrement => BinaryOp::Sub,
            };
            return Ok(Expr::PropertyUpdate {
                object: Box::new(box_obj),
                property: "v".into(),
                op: bin_op,
                prefix: *prefix,
                strict: true,
                byte_offset: 0,
                byte_end: 0,
            });
        }
        let nested;
        let rctx = if rctx.self_local.is_some()
            && matches!(
                expr,
                Expr::Closure {
                    captures_this: false,
                    ..
                }
            ) {
            nested = RewriteCtx {
                inst_id: rctx.inst_id,
                remap: None,
                func_params: rctx.func_params,
                self_local: None,
                env_local: rctx.env_local,
                env_captures: rctx.env_captures,
                boxed_param_map: rctx.boxed_param_map,
                void_return: false,
                has_expected: false,
            };
            &nested
        } else {
            rctx
        };
        let child_expected = matches!(expr, Expr::LocalSet(..))
            || (rctx.has_expected && matches!(expr, Expr::Logical { .. } | Expr::Sequence(_)));
        let child_ctx = rctx.with_expected(child_expected);
        let mut cloned = expr.clone();
        let mut orig_children: Vec<&'h Expr> = Vec::new();
        perry_hir::walker::walk_expr_children(expr, &mut |child| orig_children.push(child));
        let (mut idx, mut err) = (0usize, None);
        perry_hir::walker::walk_expr_children_mut(&mut cloned, &mut |child| {
            if err.is_none()
                && let Some(&orig) = orig_children.get(idx)
            {
                idx += 1;
                match self.rewrite_expr(&child_ctx, orig) {
                    Ok(r) => *child = r,
                    Err(e) => err = Some(e),
                }
            }
        });
        match err {
            Some(e) => Err(e),
            None => Ok(cloned),
        }
    }

    fn rewrite_call(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: &'h Expr,
        fid: FuncId,
        argument_types: Ty,
        pass_receiver: bool,
        mode: CalleeValue,
    ) -> Result<Expr> {
        let Expr::Call {
            callee,
            args,
            byte_offset,
            byte_end,
            ..
        } = expr
        else {
            return Err(Diagnostic::internal(
                "function call resolution requires a call expression",
            ));
        };
        let ctx = rctx.with_expected(true);
        let mut operands = Vec::with_capacity(args.len() + 2);
        let mut effects = Vec::new();
        let mut binding = None;
        let callable = if pass_receiver
            && let Expr::PropertyGet {
                object, property, ..
            } = callee.as_ref()
        {
            let receiver = self.rewrite_expr(&ctx, object)?;
            if matches!(mode, CalleeValue::Static) {
                operands.push(receiver);
                None
            } else {
                let id = self.fresh_synthetic_local()?;
                binding = Some((id, receiver));
                operands.push(Expr::LocalGet(id));
                Some(Self::prop_get(Expr::LocalGet(id), property))
            }
        } else if matches!(mode, CalleeValue::Static) {
            if !is_pure_func_expr(callee) {
                let operand = match callee.as_ref() {
                    Expr::PropertyGet { object, .. } => object.as_ref(),
                    other => other,
                };
                effects.push(self.rewrite_expr(rctx, operand)?);
            }
            None
        } else {
            Some(self.rewrite_expr(&ctx, callee)?)
        };
        if let Some(callable) = callable {
            operands.insert(
                0,
                if matches!(mode, CalleeValue::Environment) {
                    Self::prop_get(callable, "val")
                } else {
                    callable
                },
            );
        }
        effects.extend(self.rewrite_filtered_args(&ctx, args, argument_types, &mut operands)?);
        let call = sequence(
            effects,
            Self::synth_call(fid, operands, *byte_offset, *byte_end),
        );
        Ok(match binding {
            Some((id, value)) => Expr::ScopedTemp {
                id,
                value: Box::new(value),
                body: Box::new(call),
            },
            None => call,
        })
    }

    fn fold_comparison(
        &mut self,
        rctx: &RewriteCtx<'_>,
        left: &'h Expr,
        right: &'h Expr,
        result: bool,
    ) -> Result<Expr> {
        let effects = [left, right]
            .into_iter()
            .filter(|operand| !is_pure_func_expr(operand))
            .map(|operand| self.rewrite_expr(rctx, operand))
            .collect::<Result<_>>()?;
        Ok(sequence(effects, Expr::Bool(result)))
    }

    fn followed_by(&mut self, value: Expr, effects: Vec<Expr>) -> Result<Expr> {
        if effects.is_empty() {
            return Ok(value);
        }
        let id = self.fresh_synthetic_local()?;
        Ok(Expr::ScopedTemp {
            id,
            value: Box::new(value),
            body: Box::new(sequence(effects, Expr::LocalGet(id))),
        })
    }

    fn finish_operands(&mut self, values: &mut Vec<Expr>, effects: Vec<Expr>) -> Result<Vec<Expr>> {
        if !effects.is_empty()
            && let Some(last) = values.pop()
        {
            values.push(self.followed_by(last, effects)?);
            return Ok(Vec::new());
        }
        Ok(effects)
    }

    pub(super) fn erases_argument(&self, ty: Ty) -> bool {
        matches!(self.types.kind(ty), TyKind::Func(id) if !self.is_env_template(id.template))
    }

    fn rewrite_filtered_args(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        argument_types: Ty,
        values: &mut Vec<Expr>,
    ) -> Result<Vec<Expr>> {
        self.project_arguments(rctx, args, values, |emitter, i| {
            emitter
                .types
                .tuple_slice(argument_types)
                .get(i)
                .is_some_and(|&ty| !emitter.erases_argument(ty))
        })
    }

    pub(super) fn project_arguments(
        &mut self,
        rctx: &RewriteCtx<'_>,
        args: &'h [Expr],
        values: &mut Vec<Expr>,
        keep: impl Fn(&Self, usize) -> bool,
    ) -> Result<Vec<Expr>> {
        let mut effects = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if !keep(self, i) {
                if !is_pure_func_expr(arg) {
                    effects.push(self.rewrite_expr(rctx, arg)?);
                }
            } else {
                values.push(sequence(
                    std::mem::take(&mut effects),
                    self.rewrite_expr(rctx, arg)?,
                ));
            }
        }
        self.finish_operands(values, effects)
    }
}
