//! Plain expression rewriting, boxed capture access, and call operand projection.
use super::expressions::CalleeValue;
use super::*;
use crate::hir_compat;

impl<'h> Emitter<'h, '_> {
    pub(super) fn local_read_override(&self, rctx: &RewriteCtx<'_>, id: LocalId) -> Option<Expr> {
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

    pub(super) fn boxed_target(&self, rctx: &RewriteCtx<'_>, id: LocalId) -> Option<Expr> {
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

    pub(super) fn rewrite_expr_plain(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: &'h Expr,
    ) -> Result<Expr> {
        if let Expr::PrivateGuard {
            object, kind: 0, ..
        } = expr
        {
            return self.rewrite_expr(rctx, object);
        }
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
            } else if self.solved.collapsed_closures.contains(tid.index()) {
                CaptureOwner::Template(self.solved.instances[rctx.inst_id].template)
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

    pub(super) fn rewrite_call(
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
        let call = sequence(effects, synth_call(fid, operands, *byte_offset, *byte_end));
        Ok(match binding {
            Some((id, value)) => Expr::ScopedTemp {
                id,
                value: Box::new(value),
                body: Box::new(call),
            },
            None => call,
        })
    }

    pub(super) fn fold_comparison(
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

    pub(super) fn followed_by(&mut self, value: Expr, effects: Vec<Expr>) -> Result<Expr> {
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

    pub(super) fn finish_operands(
        &mut self,
        values: &mut Vec<Expr>,
        effects: Vec<Expr>,
    ) -> Result<Vec<Expr>> {
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

    pub(super) fn rewrite_filtered_args(
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
