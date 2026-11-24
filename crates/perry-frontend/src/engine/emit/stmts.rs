//! Statement and local-declaration emission from solved types.
use super::*;

impl<'h> Emitter<'h, '_> {
    pub(super) fn rewrite_condition(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: &'h Expr,
    ) -> Result<Expr> {
        match expr {
            Expr::Logical {
                op: op @ (perry_hir::ir::LogicalOp::And | perry_hir::ir::LogicalOp::Or),
                left,
                right,
            } => {
                return Ok(Expr::Logical {
                    op: *op,
                    left: Box::new(self.rewrite_condition(rctx, left)?),
                    right: Box::new(self.rewrite_condition(rctx, right)?),
                });
            }
            Expr::Sequence(parts) if let Some((last, prefix)) = parts.split_last() => {
                let mut values = Vec::with_capacity(parts.len());
                for effect in prefix {
                    values.push(self.rewrite_expr(rctx, effect)?);
                }
                values.push(self.rewrite_condition(rctx, last)?);
                return Ok(Expr::Sequence(values));
            }
            _ => {}
        }
        let rewritten = self.rewrite_expr(rctx, expr)?;
        Ok(match rewritten {
            Expr::FuncRef(_) => Expr::Bool(true),
            other => other,
        })
    }

    pub(super) fn rewrite_opt_expr(
        &mut self,
        rctx: &RewriteCtx<'_>,
        expr: Option<&'h Expr>,
    ) -> Result<Option<Expr>> {
        expr.map(|e| self.rewrite_expr(rctx, e)).transpose()
    }
    pub(super) fn rewrite_stmts(
        &mut self,
        rctx: &mut RewriteCtx<'_>,
        stmts: &'h [Stmt],
    ) -> Result<Vec<Stmt>> {
        let mut out = Vec::with_capacity(stmts.len());
        for stmt in stmts {
            if let Stmt::Let { id, name, ty, .. } = stmt
                && self.is_forward_boxed(*id)
            {
                let out_id = match &mut rctx.remap {
                    Some(r) => self.fresh_local(*id, r)?,
                    None => *id,
                };
                let inner_ty = self.resolve_let_ty(rctx.inst_id, *id, out_id, name, ty)?;
                let box_ty = box_hir(Self::admitting_undefined_hir(inner_ty));
                self.boxed_locals.push(out_id);
                out.push(Stmt::Let {
                    id: out_id,
                    name: name.clone(),
                    ty: box_ty,
                    mutable: false,
                    init: Some(Self::emit_box_record(Expr::Undefined)),
                });
            }
        }
        let mut pending_for_boxes: Vec<LocalId> = Vec::new();
        for stmt in stmts {
            match stmt {
                Stmt::Let { id, .. }
                    if self.index.is_for_head_local(*id) && self.is_boxed_local(*id) =>
                {
                    let rewritten = self.rewrite_stmt(rctx, stmt)?;
                    if let Stmt::Let { id: out_id, .. } = &rewritten {
                        pending_for_boxes.push(*out_id);
                    }
                    out.push(rewritten);
                }
                Stmt::For {
                    init,
                    condition,
                    update,
                    body,
                } => {
                    let mut box_ids = std::mem::take(&mut pending_for_boxes);
                    let rewritten_init = init
                        .as_deref()
                        .map(|s| {
                            let r = self.rewrite_stmt(rctx, s)?;
                            if let (Stmt::Let { id: orig_id, .. }, Stmt::Let { id: out_id, .. }) =
                                (s, &r)
                                && self.index.is_for_head_local(*orig_id)
                                && self.is_boxed_local(*orig_id)
                            {
                                box_ids.push(*out_id);
                            }
                            Ok::<_, Diagnostic>(Box::new(r))
                        })
                        .transpose()?;
                    let rewritten_cond = self.rewrite_opt_expr(rctx, condition.as_ref())?;
                    let mut rewritten_update = self.rewrite_opt_expr(rctx, update.as_ref())?;
                    let rewritten_body = self.rewrite_stmts(rctx, body)?;
                    if !box_ids.is_empty() {
                        let mut seq = Vec::with_capacity(box_ids.len() + 1);
                        for box_id in box_ids {
                            let fresh =
                                Self::emit_box_record(Self::prop_get(Expr::LocalGet(box_id), "v"));
                            seq.push(Expr::LocalSet(box_id, Box::new(fresh)));
                        }
                        if let Some(u) = rewritten_update {
                            seq.push(u);
                        }
                        rewritten_update = Some(Expr::Sequence(seq));
                    }
                    out.push(Stmt::For {
                        init: rewritten_init,
                        condition: rewritten_cond,
                        update: rewritten_update,
                        body: rewritten_body,
                    });
                }
                _ => {
                    pending_for_boxes.clear();
                    out.push(self.rewrite_stmt(rctx, stmt)?);
                }
            }
        }
        Ok(out)
    }

    pub(super) fn resolve_let_ty(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        out_id: LocalId,
        name: &str,
        ty: &HirType,
    ) -> Result<HirType> {
        let resolved = self.local_ty(inst_id, id);
        let is_inferred = !self.hir.local_source_spans.contains_key(&id)
            || self
                .index
                .local_idx(id)
                .is_none_or(|li| self.index.local_inferred.contains(li.index()));
        let is_deferred = self
            .index
            .local_idx(id)
            .is_some_and(|li| self.index.local_deferred.contains(li.index()));
        match resolved {
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(ty, HirType::Any | HirType::Unknown)
                        || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                        || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
            {
                if is_inferred
                    && is_deferred
                    && self.types.is_json_encodable(
                        self.ctx,
                        t,
                        self.solved.locations.as_slice(),
                        &self.solved.dict_reads,
                    )
                {
                    self.json_eligible_bindings.push(out_id);
                }
                self.materialize(t)
            }
            Some(t)
                if matches!(
                    self.types.kind(t),
                    TyKind::Cell {
                        kind: CellKind::Dict,
                        ..
                    }
                ) =>
            {
                self.materialize(t)
            }
            Some(t)
                if let TyKind::Cell { loc, kind } = self.types.kind(t)
                    && kind.is_array()
                    && !self.is_resolved(t) =>
            {
                let mut err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "an empty array literal needs a declared element type"
                )
                .with_help(format!(
                    "Add a concrete element type annotation to '{name}'."
                ));
                if self.solved.self_nested_locs.contains(loc.index()) {
                    err = err.with_note("its inferred element type nests itself");
                }
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            Some(Ty::OPEN)
                if is_inferred
                    && self.index.local_idx(id).is_some_and(|lidx| {
                        self.solved.self_nested_locals.contains(lidx.index())
                    }) =>
            {
                let err = diag!(
                    codes::MISSING_TYPE_ANNOTATION,
                    "cannot infer type of '{name}': inferred type nests itself"
                )
                .with_help("Add a concrete type annotation to this binding.");
                let err = self.annotate_local(err, id);
                Err(self.attach_inst_chain(err, inst_id))
            }
            _ => Ok(ty.clone()),
        }
    }

    pub(super) fn rewrite_stmt(
        &mut self,
        rctx: &mut RewriteCtx<'_>,
        stmt: &'h Stmt,
    ) -> Result<Stmt> {
        match stmt {
            Stmt::Let {
                id,
                name,
                ty,
                mutable,
                init,
            } => {
                let out_id = match &mut rctx.remap {
                    Some(r) => self.fresh_local(*id, r)?,
                    None => *id,
                };
                let resolved = self.local_ty(rctx.inst_id, *id);
                if let Some(t) = resolved
                    && let TyKind::Func(ident) = self.types.kind(t)
                    && !self.is_env_template(ident.template)
                {
                    if let Some(init_expr) = init
                        && !is_pure_func_expr(init_expr)
                    {
                        return Ok(Stmt::Expr(self.rewrite_expr(rctx, init_expr)?));
                    }
                    let fid = self.find_func_identity_instance(ident).ok_or_else(|| {
                        Diagnostic::internal(format!(
                            "unresolved function identity for binding '{name}'"
                        ))
                    })?;
                    return Ok(Stmt::Let {
                        id: out_id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: *mutable,
                        init: Some(Expr::FuncRef(fid)),
                    });
                }
                if self.is_forward_boxed(*id) {
                    let init_ctx = rctx.with_expected(true);
                    return match self.rewrite_opt_expr(&init_ctx, init.as_ref())? {
                        Some(val) => Ok(Stmt::Expr(Expr::PropertySet {
                            object: Box::new(Expr::LocalGet(out_id)),
                            property: "v".into(),
                            value: Box::new(val),
                            byte_offset: 0,
                            byte_end: 0,
                        })),
                        None => Ok(Stmt::Expr(Expr::Undefined)),
                    };
                }
                let new_ty = self.resolve_let_ty(rctx.inst_id, *id, out_id, name, ty)?;
                let init_has_expected = !matches!(new_ty, HirType::Any | HirType::Unknown);
                let init_ctx = rctx.with_expected(init_has_expected);
                let rewritten_init = self.rewrite_opt_expr(&init_ctx, init.as_ref())?;
                if self.is_boxed_local(*id) {
                    self.boxed_locals.push(out_id);
                    let undef_box = self.is_undef_boxed(*id);
                    let (box_inner_ty, init_val) =
                        if undef_box && matches!(init, None | Some(Expr::Undefined)) {
                            (Self::admitting_undefined_hir(new_ty), Expr::Undefined)
                        } else {
                            let v = match rewritten_init {
                                Some(v) => v,
                                None => self.default_box_init(&new_ty, *id, rctx.inst_id)?,
                            };
                            let t = if undef_box {
                                Self::admitting_undefined_hir(new_ty)
                            } else {
                                new_ty
                            };
                            (t, v)
                        };
                    return Ok(Stmt::Let {
                        id: out_id,
                        name: name.clone(),
                        ty: box_hir(box_inner_ty),
                        mutable: self.index.is_for_head_local(*id),
                        init: Some(Self::emit_box_record(init_val)),
                    });
                }
                Ok(Stmt::Let {
                    id: out_id,
                    name: name.clone(),
                    ty: new_ty,
                    mutable: *mutable,
                    init: rewritten_init,
                })
            }
            Stmt::Expr(e) => Ok(Stmt::Expr(self.rewrite_expr(rctx, e)?)),
            Stmt::Throw(e) => Ok(Stmt::Throw(self.rewrite_expr(rctx, e)?)),
            Stmt::Return(v) if rctx.void_return => {
                if let Some(e) = v
                    && !is_pure_func_expr(e)
                {
                    let eff = self.rewrite_expr(rctx, e)?;
                    return Ok(Stmt::Return(Some(Expr::Sequence(vec![
                        eff,
                        Expr::Undefined,
                    ]))));
                }
                Ok(Stmt::Return(None))
            }
            Stmt::Return(v) => {
                let ret_ctx = rctx.with_expected(true);
                Ok(Stmt::Return(self.rewrite_opt_expr(&ret_ctx, v.as_ref())?))
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Ok(Stmt::If {
                condition: self.rewrite_condition(rctx, condition)?,
                then_branch: self.rewrite_stmts(rctx, then_branch)?,
                else_branch: else_branch
                    .as_deref()
                    .map(|b| self.rewrite_stmts(rctx, b))
                    .transpose()?,
            }),
            Stmt::While { condition, body } => Ok(Stmt::While {
                condition: self.rewrite_condition(rctx, condition)?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::DoWhile { condition, body } => Ok(Stmt::DoWhile {
                condition: self.rewrite_condition(rctx, condition)?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => Ok(Stmt::For {
                init: init
                    .as_deref()
                    .map(|s| self.rewrite_stmt(rctx, s).map(Box::new))
                    .transpose()?,
                condition: self.rewrite_opt_expr(rctx, condition.as_ref())?,
                update: self.rewrite_opt_expr(rctx, update.as_ref())?,
                body: self.rewrite_stmts(rctx, body)?,
            }),
            Stmt::Try {
                body,
                catch,
                finally,
                forof_completion,
            } => Ok(Stmt::Try {
                body: self.rewrite_stmts(rctx, body)?,
                catch: catch
                    .as_ref()
                    .map(|c| {
                        let param = match (&c.param, &mut rctx.remap) {
                            (Some((id, name)), Some(r)) => {
                                Some((self.fresh_local(*id, r)?, name.clone()))
                            }
                            (other, _) => other.clone(),
                        };
                        Ok::<_, Diagnostic>(CatchClause {
                            param,
                            body: self.rewrite_stmts(rctx, &c.body)?,
                        })
                    })
                    .transpose()?,
                finally: finally
                    .as_deref()
                    .map(|f| self.rewrite_stmts(rctx, f))
                    .transpose()?,
                forof_completion: *forof_completion,
            }),
            Stmt::Switch {
                discriminant,
                cases,
            } => Ok(Stmt::Switch {
                discriminant: self.rewrite_expr(rctx, discriminant)?,
                cases: cases
                    .iter()
                    .map(|c| {
                        Ok(SwitchCase {
                            test: self.rewrite_opt_expr(rctx, c.test.as_ref())?,
                            body: self.rewrite_stmts(rctx, &c.body)?,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            }),
            Stmt::Labeled { label, body } => Ok(Stmt::Labeled {
                label: label.clone(),
                body: Box::new(self.rewrite_stmt(rctx, body)?),
            }),
            other => Ok(other.clone()),
        }
    }

    pub(super) fn default_box_init(
        &self,
        ty: &HirType,
        id: LocalId,
        inst_id: InstanceId,
    ) -> Result<Expr> {
        match ty {
            HirType::Void => Ok(Expr::Undefined),
            HirType::Null => Ok(Expr::Null),
            HirType::Union(members) if members.contains(&HirType::Void) => Ok(Expr::Undefined),
            HirType::Union(members) if members.contains(&HirType::Null) => Ok(Expr::Null),
            HirType::Number | HirType::Int32 => Ok(Expr::Number(0.0)),
            HirType::Boolean => Ok(Expr::Bool(false)),
            HirType::String => Ok(Expr::String(String::new())),
            _ => {
                let err = self.annotate_local(
                    Diagnostic::unsupported(
                        "captured local declared without an initializer has no placeholder value",
                    ),
                    id,
                );
                Err(self.attach_inst_chain(err, inst_id))
            }
        }
    }
}
