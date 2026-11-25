//! Emit expressions from solved call sites and function-value coercions.
use super::*;

pub(super) enum CalleeValue {
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
                FuncCoercion::Tag(ident, target) if is_pure_func_expr(expr) => {
                    return self.expression_op(
                        crate::engine::ExpressionOp::Function {
                            identity: ident,
                            target,
                        },
                        vec![],
                    );
                }
                _ => {}
            }
        }
        let rewritten = self.rewrite_expr_inner(rctx, expr, resolution)?;
        match coercion {
            Some(FuncCoercion::Zero) => Ok(Expr::Sequence(vec![rewritten, Expr::Number(0.0)])),
            Some(FuncCoercion::Tag(ident, target)) => {
                let tag = self.expression_op(
                    crate::engine::ExpressionOp::Function {
                        identity: ident,
                        target,
                    },
                    vec![],
                )?;
                Ok(Expr::Sequence(vec![rewritten, tag]))
            }
            Some(FuncCoercion::BoxEnv(ident, target)) => self.expression_op(
                crate::engine::ExpressionOp::Function {
                    identity: ident,
                    target,
                },
                vec![rewritten],
            ),
            Some(FuncCoercion::Widen(helper)) => {
                let fid = self.emit_helper(helper)?;
                Ok(synth_call(fid, vec![rewritten], 0, 0))
            }
            Some(FuncCoercion::EnvCallback { target, input_tys }) => {
                let target_fid = self.solved.instances[target].emitted_func_id;
                let inputs = self.types.tuple_slice(input_tys);
                self.bind_callback(
                    target_fid,
                    crate::engine::CallbackOperand::Environment,
                    rewritten,
                    inputs,
                )
            }
            Some(FuncCoercion::DispatchCallback {
                helper,
                arms,
                input_tys,
            }) => {
                let arm_slice = &self.solved.dispatch_arms[arms];
                let inputs = self.types.tuple_slice(input_tys);
                let max_params = arm_slice
                    .iter()
                    .map(|&(ident, _)| self.index.view(ident.template))
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
                let fid = self.emit_helper(helper)?;
                self.bind_callback(
                    fid,
                    crate::engine::CallbackOperand::Callee,
                    rewritten,
                    &inputs[..max_params],
                )
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
                    self.prepare_layouts(ty)?;
                    return self
                        .expression_op(crate::engine::ExpressionOp::Expected(ty), vec![value]);
                }
                SiteResolution::Cell(_) => {}
                SiteResolution::FoldBool(b) => {
                    if let Expr::Compare { left, right, .. } = expr {
                        return self.fold_comparison(rctx, left, right, b);
                    }
                }
                SiteResolution::CollectionNew { span, ty } => {
                    let owner = (rctx.inst_id != self.solved.init_inst)
                        .then(|| self.solved.instances[rctx.inst_id].emitted_func_id);
                    self.prepare_layouts(ty)?;
                    self.plans.collection_types.insert((owner, span), ty);
                }
                SiteResolution::ThrowTypeError(helper) => {
                    let fid = self.emit_helper(helper)?;
                    let call = synth_call(fid, Vec::new(), 0, 0);
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
                        return Ok(sequence(effects, call));
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
                        return Ok(synth_call(fid, vec![arg], 0, 0));
                    }
                }
                SiteResolution::Callback { target } => {
                    return Ok(Expr::FuncRef(self.solved.instances[target].emitted_func_id));
                }
                SiteResolution::DirectCall {
                    target,
                    argument_types,
                    pass_receiver,
                    pass_env,
                } => {
                    let fid = self.solved.instances[target].emitted_func_id;
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
                                let comparison = self.expression_op(
                                    crate::engine::ExpressionOp::Matches {
                                        identity: ident,
                                        source: union_ty,
                                        equal: is_eq,
                                    },
                                    vec![u_rewritten],
                                )?;
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
                        let call = synth_call(cmp_fid, vec![l_rewritten, r_rewritten], 0, 0);
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
                            .bodies
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
                    self.prepare_layouts(ty)?;
                    return self
                        .expression_op(crate::engine::ExpressionOp::Expected(ty), vec![array]);
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
                        return Ok(sequence(effects, Expr::Undefined));
                    }
                }
                SiteResolution::MethodCall {
                    target,
                    argument_types,
                } => {
                    let method_name = self.solved.instances[target].emitted_method_name.clone();
                    match expr {
                        Expr::StaticMethodCall {
                            class_name, args, ..
                        } => {
                            let mut out_args = Vec::with_capacity(args.len());
                            let before = self.rewrite_filtered_args(
                                rctx,
                                args,
                                argument_types,
                                &mut out_args,
                            )?;
                            return Ok(sequence(
                                before,
                                Expr::StaticMethodCall {
                                    class_name: class_name.clone(),
                                    method_name,
                                    args: out_args,
                                },
                            ));
                        }
                        Expr::Call {
                            callee,
                            args,
                            byte_offset,
                            byte_end,
                            ..
                        } if let Expr::PropertyGet {
                            object,
                            byte_offset: prop_offset,
                            byte_end: prop_end,
                            ..
                        } = callee.as_ref() =>
                        {
                            let mut out_args = Vec::with_capacity(args.len());
                            let effects = self.rewrite_filtered_args(
                                rctx,
                                args,
                                argument_types,
                                &mut out_args,
                            )?;
                            let receiver = self.rewrite_expr(rctx, object)?;
                            return Ok(Expr::Call {
                                callee: Box::new(Expr::PropertyGet {
                                    object: Box::new(self.followed_by(receiver, effects)?),
                                    property: method_name,
                                    byte_offset: *prop_offset,
                                    byte_end: *prop_end,
                                }),
                                args: out_args,
                                type_args: Vec::new(),
                                byte_offset: *byte_offset,
                                byte_end: *byte_end,
                            });
                        }
                        _ => {}
                    }
                }
                SiteResolution::ClassNew {
                    class_inst,
                    argument_types,
                } => {
                    if let Expr::New {
                        args,
                        byte_offset,
                        byte_end,
                        cap_args_appended,
                        ..
                    } = expr
                    {
                        let mut out_args = Vec::with_capacity(args.len());
                        let before =
                            self.rewrite_filtered_args(rctx, args, argument_types, &mut out_args)?;
                        return Ok(sequence(
                            before,
                            Expr::New {
                                class_name: self.solved.class_insts[class_inst]
                                    .emitted_name
                                    .clone(),
                                args: out_args,
                                type_args: Vec::new(),
                                byte_offset: *byte_offset,
                                byte_end: *byte_end,
                                cap_args_appended: *cap_args_appended,
                            },
                        ));
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
                            synth_call(fid, call_args, *byte_offset, *byte_end),
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
}
