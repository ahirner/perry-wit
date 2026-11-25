//! Plain expression rewriting, boxed capture access, and call operand projection.
use super::expressions::CalleeValue;
use super::*;

impl<'h> Emitter<'h, '_> {
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
            let constructor = self.environment(ident)?;
            let fields = self.env_layout(ident)?;
            let args = if fields.is_empty() {
                vec![Expr::Number(0.0)]
            } else {
                let mut out_args = Vec::with_capacity(fields.len());
                for (cap_id, _, dst_ty, boxed) in fields {
                    let mut arg = Expr::LocalGet(cap_id);
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
            return Ok(synth_call(constructor, args, 0, 0));
        }
        if let Expr::LocalGet(id) = expr
            && let Some((_, fid)) = rctx.func_params.iter().find(|(lid, _)| lid == id)
        {
            return Ok(Expr::FuncRef(*fid));
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
