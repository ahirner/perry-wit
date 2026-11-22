use super::*;
use walk::Visitor;

pub(super) fn argument(
    arg: &Expr,
    ty: HirType,
    guards: &Guards,
    captured_writes: &HashSet<LocalId>,
) -> HirType {
    let Expr::LocalGet(id) = arg else {
        return ty;
    };
    if captured_writes.contains(id) {
        return ty;
    }
    let Some(excluded) = guards.excluded(*id) else {
        return ty;
    };
    let HirType::Union(mut items) = ty else {
        return ty;
    };
    items.retain(|ty| match ty {
        HirType::Null => !excluded.admits_null(),
        HirType::Void => !excluded.admits_undefined(),
        _ => true,
    });
    match items.as_slice() {
        [] => HirType::Never,
        [_] => items.swap_remove(0),
        _ => HirType::Union(items),
    }
}

pub(super) struct Calls<'a, 's, F> {
    env: &'a mut FunctionEnv<'s>,
    guards: Guards<'static>,
    call: F,
}

impl<'s, F: FnMut(&mut Expr, &Guards, &mut FunctionEnv<'s>) -> Result<()>> Visitor
    for Calls<'_, 's, F>
{
    type Error = Diagnostic;

    fn node(&mut self, node: Node<'_>) -> Result<()> {
        if let Node::Expression(expr) = node {
            if let Expr::LocalSet(id, _) | Expr::Update { id, .. } = expr {
                self.guards.assign(*id);
            }
            (self.call)(expr, &self.guards, self.env)?;
        }
        Ok(())
    }

    fn body(&mut self, body: &mut [Stmt]) -> Result<()> {
        let depth = self.guards.depth();
        for statement in body {
            self.statement(statement)?;
        }
        self.guards.truncate(depth);
        Ok(())
    }

    fn expression(&mut self, expr: &mut Expr) -> Result<()> {
        closures::bind_callback(expr, self.env);
        match expr {
            Expr::Conditional {
                condition,
                then_expr,
                else_expr,
            } => {
                self.expression(condition)?;
                self.arms(condition, true, true, |v, arm| {
                    v.expression(if arm { then_expr } else { else_expr })
                })?;
                Ok(())
            }
            Expr::Logical {
                op: op @ (LogicalOp::And | LogicalOp::Or),
                left,
                right,
            } => {
                self.expression(left)?;
                self.arms(left, true, true, |v, arm| {
                    if arm == (*op == LogicalOp::And) {
                        v.expression(right)
                    } else {
                        Ok(())
                    }
                })?;
                Ok(())
            }
            _ => walk::visit_expr(expr, self),
        }
    }

    fn statement(&mut self, statement: &mut Stmt) -> Result<()> {
        match statement {
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.expression(condition)?;
                let otherwise = else_branch.as_deref_mut().unwrap_or_default();
                let then_reaches = crate::completion::falls_through(then_branch);
                let else_reaches = crate::completion::falls_through(otherwise);
                if let Some(truth) =
                    self.arms(condition, then_reaches, else_reaches, |v, arm| {
                        v.body(if arm { then_branch } else { otherwise })
                    })?
                {
                    self.guards.exclude_absences(condition, truth);
                    self.guards.assign_in(
                        if then_reaches { then_branch } else { otherwise },
                        [],
                        |_| None,
                    );
                }
            }
            Stmt::While { condition, body } => self.loop_body(Some(condition), None, body)?,
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    self.statement(init)?;
                }
                self.loop_body(condition.as_mut(), update.as_mut(), body)?;
            }
            Stmt::DoWhile { condition, body } => {
                self.guards.assign_in(body, [&*condition], |_| None);
                self.body(body)?;
                self.expression(condition)?;
            }
            _ => return walk::visit_stmt(statement, self),
        }
        Ok(())
    }
}
impl<'a, 's, F: FnMut(&mut Expr, &Guards, &mut FunctionEnv<'s>) -> Result<()>> Calls<'a, 's, F> {
    pub(super) fn new(env: &'a mut FunctionEnv<'s>, call: F) -> Self {
        Self {
            env,
            guards: Guards::default(),
            call,
        }
    }

    fn arms(
        &mut self,
        condition: &Expr,
        then_reaches: bool,
        else_reaches: bool,
        mut visit: impl FnMut(&mut Self, bool) -> Result<()>,
    ) -> Result<Option<bool>> {
        let branch = self.guards.branch();
        self.guards.exclude_absences(condition, true);
        visit(self, true)?;
        let then_writes = self.guards.other_arm(branch);
        self.guards.exclude_absences(condition, false);
        visit(self, false)?;
        Ok(self
            .guards
            .join(branch, then_writes, then_reaches, else_reaches))
    }

    fn loop_body(
        &mut self,
        test: Option<&mut Expr>,
        update: Option<&mut Expr>,
        body: &mut [Stmt],
    ) -> Result<()> {
        self.guards.assign_in(
            body,
            test.as_deref().into_iter().chain(update.as_deref()),
            |_| None,
        );
        let depth = self.guards.depth();
        if let Some(test) = test {
            self.expression(test)?;
            self.guards.exclude_absences(test, true);
        }
        self.body(body)?;
        self.guards.truncate(depth);
        if let Some(update) = update {
            self.expression(update)?;
        }
        Ok(())
    }
}
