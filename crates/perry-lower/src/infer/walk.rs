use super::*;
use perry_hir::walker::walk_expr_children_mut;

pub(super) enum Node<'a> {
    Binding(&'a mut LocalId, &'a mut HirType),
    Local(&'a mut LocalId),
    Type(&'a mut HirType),
    Expression(&'a mut Expr),
}

pub(super) fn visit_expr(expr: &mut Expr, visit: &mut impl Visitor) -> Result<()> {
    match expr {
        Expr::Call { type_args, .. }
        | Expr::CallSpread { type_args, .. }
        | Expr::New { type_args, .. } => {
            for ty in type_args {
                visit.node(Node::Type(ty))?;
            }
        }
        Expr::JsonParseTyped { ty, .. }
        | Expr::PodLayoutSizeOf { ty, .. }
        | Expr::PodLayoutAlignOf { ty, .. }
        | Expr::PodLayoutOffsetOf { ty, .. } => visit.node(Node::Type(ty))?,
        Expr::ExternFuncRef {
            param_types,
            return_type,
            ..
        } => {
            for ty in param_types {
                visit.node(Node::Type(ty))?;
            }
            visit.node(Node::Type(return_type))?;
        }
        Expr::ScopedTemp { id, .. } => visit.node(Node::Local(id))?,
        Expr::Closure {
            params,
            return_type,
            body,
            ..
        } => {
            for param in params {
                visit.node(Node::Binding(&mut param.id, &mut param.ty))?;
            }
            visit.node(Node::Type(return_type))?;
            visit_body(body, visit)?;
        }
        _ => {}
    }
    let mut result = Ok(());
    walk_expr_children_mut(expr, &mut |child| {
        if result.is_ok() {
            result = visit.expression(child);
        }
    });
    result?;
    visit.node(Node::Expression(expr))
}

pub(super) fn visit_body(statements: &mut [Stmt], visit: &mut impl Visitor) -> Result<()> {
    for statement in statements {
        match statement {
            Stmt::Let { id, ty, init, .. } => {
                visit.node(Node::Binding(id, ty))?;
                if let Some(init) = init {
                    visit.expression(init)?;
                }
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) => visit.expression(expr)?,
            Stmt::Return(Some(value)) => visit.expression(value)?,
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                visit.expression(condition)?;
                visit_body(then_branch, visit)?;
                if let Some(body) = else_branch {
                    visit_body(body, visit)?;
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                visit.expression(condition)?;
                visit_body(body, visit)?;
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    visit_body(std::slice::from_mut(init), visit)?;
                }
                if let Some(condition) = condition {
                    visit.expression(condition)?;
                }
                if let Some(update) = update {
                    visit.expression(update)?;
                }
                visit_body(body, visit)?;
            }
            Stmt::Labeled { body, .. } => visit_body(std::slice::from_mut(body), visit)?,
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                visit_body(body, visit)?;
                if let Some(catch) = catch {
                    if let Some((id, _)) = &mut catch.param {
                        visit.node(Node::Local(id))?;
                    }
                    visit_body(&mut catch.body, visit)?;
                }
                if let Some(body) = finally {
                    visit_body(body, visit)?;
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                visit.expression(discriminant)?;
                for case in cases {
                    if let Some(test) = &mut case.test {
                        visit.expression(test)?;
                    }
                    visit_body(&mut case.body, visit)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) trait Visitor: Sized {
    fn node(&mut self, node: Node<'_>) -> Result<()>;

    fn expression(&mut self, expr: &mut Expr) -> Result<()> {
        visit_expr(expr, self)
    }
}

impl<F: FnMut(Node<'_>) -> Result<()>> Visitor for F {
    fn node(&mut self, node: Node<'_>) -> Result<()> {
        self(node)
    }
}

pub(super) fn function(
    function: &mut perry_hir::ir::Function,
    visit: &mut impl Visitor,
) -> Result<()> {
    for param in &mut function.params {
        visit.node(Node::Binding(&mut param.id, &mut param.ty))?;
        if let Some(default) = &mut param.default {
            visit.expression(default)?;
        }
    }
    visit.node(Node::Type(&mut function.return_type))?;
    visit_body(&mut function.body, visit)
}

pub(super) fn locals_mut(statements: &mut [Stmt], visit: &mut impl FnMut(LocalId, &mut HirType)) {
    struct Locals<'a, F>(&'a mut F);
    impl<F: FnMut(LocalId, &mut HirType)> Visitor for Locals<'_, F> {
        fn node(&mut self, node: Node<'_>) -> Result<()> {
            if let Node::Binding(id, ty) = node {
                self.0(*id, ty);
            }
            Ok(())
        }
        fn expression(&mut self, _: &mut Expr) -> Result<()> {
            Ok(())
        }
    }
    // Locals never produces an error or descends into nested function scopes.
    let _ = visit_body(statements, &mut Locals(visit));
}

pub(super) fn resolve_locals(statements: &mut [Stmt], env: &mut FunctionEnv<'_>) {
    let mut bindings = HashMap::new();
    scan(statements, env, &mut ScanMode::ResolveLocals(&mut bindings));
    locals_mut(statements, &mut |id, ty| {
        if let Some(inferred) = bindings.remove(&id) {
            *ty = inferred;
        }
    });
}

pub(super) fn scan(statements: &[Stmt], env: &mut FunctionEnv<'_>, mode: &mut ScanMode<'_>) {
    for statement in statements {
        match statement {
            Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } if (matches!(ty, HirType::Any | HirType::Unknown)
                || matches!(
                    ty,
                    HirType::Generic { base, type_args }
                        if matches!(base.as_str(), "Map" | "Set") && type_args.is_empty()
                ))
                && !matches!(init, Expr::Undefined) =>
            {
                let inferred = expression(init, env);
                if let ScanMode::ResolveLocals(bindings) = mode
                    && is_resolvable_type(&inferred)
                {
                    bindings.insert(*id, inferred.clone());
                }
                if !matches!(inferred, HirType::Any | HirType::Unknown) {
                    env.locals.insert(*id, inferred);
                }
            }
            Stmt::Let {
                init: Some(init), ..
            } => {
                let _ = expression(init, env);
            }
            Stmt::Expr(Expr::LocalSet(id, value))
                if env
                    .base
                    .local_type(*id)
                    .is_none_or(|ty| matches!(ty, HirType::Any | HirType::Unknown))
                    && !env.locals.contains_key(id) =>
            {
                let inferred = expression(value, env);
                if !matches!(inferred, HirType::Any | HirType::Unknown | HirType::Void) {
                    env.locals.insert(*id, inferred);
                }
            }
            Stmt::Expr(expr) | Stmt::Throw(expr) => {
                let _ = expression(expr, env);
            }
            Stmt::Return(value) => {
                let ret_ty = value
                    .as_ref()
                    .map_or(HirType::Void, |value| expression(value, env));
                if let ScanMode::Returns(returned) = mode {
                    returned.push(ret_ty);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let _ = expression(condition, env);
                scan(then_branch, env, mode);
                if let Some(body) = else_branch {
                    scan(body, env, mode);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let _ = expression(condition, env);
                scan(body, env, mode);
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    scan(std::slice::from_ref(init), env, mode);
                }
                if let Some(condition) = condition {
                    let _ = expression(condition, env);
                }
                if let Some(update) = update {
                    let _ = expression(update, env);
                }
                scan(body, env, mode);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                scan(body, env, mode);
                if let Some(catch) = catch {
                    scan(&catch.body, env, mode);
                }
                if let Some(body) = finally {
                    scan(body, env, mode);
                }
            }
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                let _ = expression(discriminant, env);
                for case in cases {
                    if let Some(test) = &case.test {
                        let _ = expression(test, env);
                    }
                    scan(&case.body, env, mode);
                }
            }
            Stmt::Labeled { body, .. } => scan(std::slice::from_ref(body), env, mode),
            _ => {}
        }
    }
}
