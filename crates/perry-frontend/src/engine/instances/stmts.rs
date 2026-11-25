//! Local storage and statement contexts for each solved instance.
use super::*;

impl<'h> InstancePlanner<'h, '_> {
    fn plan_condition(&mut self, scope: &InstanceContext<'_>, expr: &'h Expr) -> Result<()> {
        match expr {
            Expr::Logical {
                op: perry_hir::ir::LogicalOp::And | perry_hir::ir::LogicalOp::Or,
                left,
                right,
            } => {
                self.plan_condition(scope, left)?;
                self.plan_condition(scope, right)
            }
            Expr::Sequence(parts) if let Some((last, prefix)) = parts.split_last() => {
                for effect in prefix {
                    self.plan_expr(scope, effect)?;
                }
                self.plan_condition(scope, last)
            }
            _ => self.plan_expr(scope, expr),
        }
    }

    pub(super) fn plan_stmts(
        &mut self,
        scope: &InstanceContext<'_>,
        stmts: &'h [Stmt],
    ) -> Result<()> {
        for stmt in stmts {
            self.plan_stmt(scope, stmt)?;
        }
        Ok(())
    }

    pub(super) fn local_storage(
        &mut self,
        instance: InstanceId,
        id: LocalId,
        storage: crate::engine::Storage,
    ) {
        let owner = self.owner(instance);
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
        let is_inferred = !self.declarations.locals.contains_key(&id)
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

    fn plan_stmt(&mut self, scope: &InstanceContext<'_>, stmt: &'h Stmt) -> Result<()> {
        match stmt {
            Stmt::Let {
                id, name, ty, init, ..
            } => {
                if let Some((_, source)) = self.declarations.for_of(Some(stmt)) {
                    return self.plan_expr(scope, source);
                }
                if let Some(t) = self.local_ty(scope.inst_id, *id)
                    && let TyKind::Func(identity) = self.types.kind(t)
                    && !self.is_env_template(identity.template)
                {
                    self.local_storage(scope.inst_id, *id, crate::engine::Storage::ErasedFunction);
                    if let Some(init) = init
                        && !is_pure_func_expr(init)
                    {
                        self.plan_expr(scope, init)?;
                    }
                    return Ok(());
                }
                let solved = self.resolve_let_ty(scope.inst_id, *id, name, ty)?;
                if self.is_boxed_local(*id) {
                    if self.index.is_for_head_local(*id) {
                        self.plans.per_iteration.insert(*id);
                    }
                    let ty = self.boxed_type(*id, solved)?;
                    self.local_storage(
                        scope.inst_id,
                        *id,
                        crate::engine::Storage::Boxed {
                            ty,
                            init: self.cell_init(*id),
                        },
                    );
                } else if let Some(ty) = solved {
                    self.local_storage(scope.inst_id, *id, crate::engine::Storage::Value(ty));
                }
                if let Some(init) = init {
                    let expected = solved.map_or_else(
                        || !matches!(ty, HirType::Any | HirType::Unknown),
                        |ty| !matches!(ty, Ty::DYNAMIC | Ty::OPEN | Ty::BOTTOM),
                    );
                    self.plan_expr(&scope.with_expected(expected), init)?;
                }
            }
            Stmt::Return(Some(expr)) => {
                if !scope.void_return || !is_pure_func_expr(expr) {
                    self.plan_expr(&scope.with_expected(!scope.void_return), expr)?;
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.plan_condition(scope, condition)?;
                self.plan_stmts(scope, then_branch)?;
                if let Some(body) = else_branch {
                    self.plan_stmts(scope, body)?;
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                self.plan_condition(scope, condition)?;
                self.plan_stmts(scope, body)?;
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.plan_stmt(scope, init)?;
                }
                if let Some(condition) = condition {
                    self.plan_condition(scope, condition)?;
                }
                if let Some(update) = update {
                    self.plan_expr(scope, update)?;
                }
                self.plan_stmts(scope, body)?;
            }
            Stmt::Try {
                body,
                catch,
                finally,
                ..
            } => {
                self.plan_stmts(scope, body)?;
                if let Some(catch) = catch {
                    self.plan_stmts(scope, &catch.body)?;
                }
                if let Some(body) = finally {
                    self.plan_stmts(scope, body)?;
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                self.plan_expr(scope, discriminant)?;
                for case in cases {
                    if let Some(test) = &case.test {
                        self.plan_expr(scope, test)?;
                    }
                    self.plan_stmts(scope, &case.body)?;
                }
            }
            Stmt::Labeled { body, .. } => self.plan_stmt(scope, body)?,
            Stmt::Expr(expr) | Stmt::Throw(expr) => self.plan_expr(scope, expr)?,
            Stmt::Return(None)
            | Stmt::Break
            | Stmt::Continue
            | Stmt::LabeledBreak(_)
            | Stmt::LabeledContinue(_)
            | Stmt::PreallocateBoxes(_)
            | Stmt::PreallocateTdzBoxes(_)
            | Stmt::ReleaseBoxes(_) => {}
        }
        Ok(())
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
