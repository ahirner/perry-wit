use super::*;

pub(super) fn locals_mut(statements: &mut [Stmt], visit: &mut impl FnMut(LocalId, &mut HirType)) {
    for statement in statements {
        match statement {
            Stmt::Let { id, ty, .. } => visit(*id, ty),
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                locals_mut(then_branch, visit);
                if let Some(body) = else_branch {
                    locals_mut(body, visit);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                locals_mut(body, visit);
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    locals_mut(std::slice::from_mut(init), visit);
                }
                locals_mut(body, visit);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                locals_mut(body, visit);
                if let Some(catch) = catch {
                    locals_mut(&mut catch.body, visit);
                }
                if let Some(body) = finally {
                    locals_mut(body, visit);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    locals_mut(&mut case.body, visit);
                }
            }
            Stmt::Labeled { body, .. } => {
                locals_mut(std::slice::from_mut(body), visit);
            }
            _ => {}
        }
    }
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
