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
        rctx: &RewriteCtx<'_>,
        stmts: &'h [Stmt],
    ) -> Result<Vec<Stmt>> {
        stmts
            .iter()
            .map(|stmt| self.rewrite_stmt(rctx, stmt))
            .collect()
    }

    pub(super) fn local_storage(
        &mut self,
        instance: InstanceId,
        id: LocalId,
        storage: crate::engine::Storage,
    ) {
        let owner = (instance != self.solved.init_inst)
            .then(|| self.solved.instances[instance].emitted_func_id);
        self.plans.locals.insert((owner, id), storage);
    }

    fn resolve_let_ty(
        &mut self,
        inst_id: InstanceId,
        id: LocalId,
        name: &str,
        ty: &HirType,
    ) -> Result<Option<Ty>> {
        let resolved = self.local_ty(inst_id, id);
        let local_flags = self
            .index
            .local_idx(id)
            .map(|li| self.index.locals[li].flags);
        let is_inferred = !self.hir.declarations.local_source_spans.contains_key(&id)
            || local_flags.is_none_or(|f| f.contains(LocalFlags::INFERRED));
        match resolved {
            Some(t)
                if self.is_resolved(t)
                    && (is_inferred
                        || matches!(ty, HirType::Any | HirType::Unknown)
                        || matches!(ty, HirType::Array(e) if matches!(**e, HirType::Any | HirType::Unknown))
                        || matches!(self.types.kind(t), TyKind::ClassInst(_))) =>
            {
                self.prepare_layouts(t)?;
                Ok(Some(t))
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
                self.prepare_layouts(t)?;
                Ok(Some(t))
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
            _ => Ok(None),
        }
    }

    pub(super) fn rewrite_stmt(&mut self, rctx: &RewriteCtx<'_>, stmt: &'h Stmt) -> Result<Stmt> {
        match stmt {
            Stmt::Let {
                id,
                name,
                ty,
                mutable,
                init,
            } => {
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
                    let fid = self.find_func_identity_instance(ident);
                    return Ok(Stmt::Let {
                        id: *id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: *mutable,
                        init: Some(Expr::FuncRef(fid)),
                    });
                }
                let solved = self.resolve_let_ty(rctx.inst_id, *id, name, ty)?;
                let init_has_expected = solved.map_or_else(
                    || !matches!(ty, HirType::Any | HirType::Unknown),
                    |t| !matches!(t, Ty::DYNAMIC | Ty::OPEN | Ty::BOTTOM),
                );
                let init_ctx = rctx.with_expected(init_has_expected);
                let init = self.rewrite_opt_expr(&init_ctx, init.as_ref())?;
                if self.is_boxed_local(*id) {
                    if self.index.is_for_head_local(*id) {
                        self.plans.per_iteration.insert(*id);
                    }
                    let inner = self.boxed_type(*id, solved)?;
                    self.local_storage(
                        rctx.inst_id,
                        *id,
                        crate::engine::Storage::Boxed {
                            ty: inner,
                            init: self.cell_init(*id),
                        },
                    );
                    return Ok(Stmt::Let {
                        id: *id,
                        name: name.clone(),
                        ty: ty.clone(),
                        mutable: self.index.is_for_head_local(*id),
                        init,
                    });
                }
                if let Some(solved) = solved {
                    self.local_storage(rctx.inst_id, *id, crate::engine::Storage::Value(solved));
                }
                Ok(Stmt::Let {
                    id: *id,
                    name: name.clone(),
                    ty: ty.clone(),
                    mutable: *mutable,
                    init,
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
                        Ok::<_, Diagnostic>(CatchClause {
                            param: c.param.clone(),
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

    pub(super) fn boxed_type(&self, id: LocalId, solved: Option<Ty>) -> Result<Ty> {
        solved
            .or_else(|| {
                self.index
                    .local_idx(id)
                    .map(|local| self.index.locals[local].declared)
            })
            .ok_or_else(|| Diagnostic::internal("boxed local has no declaration"))
    }
}
