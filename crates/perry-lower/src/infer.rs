//! Conservative completion-aware inference for unresolved local helper returns.
use super::*;

fn expression(expr: &Expr, env: &perry_hir::HirTypeEnv) -> HirType {
    if let Expr::Call { callee, .. } = expr
        && let Expr::PropertyGet {
            object, property, ..
        } = &**callee
        && matches!(property.as_str(), "concat" | array_methods::CONCAT)
    {
        return perry_hir::infer_expr_type(object, env);
    }
    perry_hir::infer_expr_type(expr, env)
}

fn scan(
    statements: &mut [Stmt],
    env: &mut perry_hir::HirTypeEnv,
    returned: &mut Vec<HirType>,
    resolve: bool,
) {
    for statement in statements {
        match statement {
            Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } if matches!(ty, HirType::Any | HirType::Unknown) => {
                let inferred = expression(init, env);
                env.insert_local(*id, inferred.clone());
                if resolve
                    && !matches!(
                        inferred,
                        HirType::Any | HirType::Unknown | HirType::Void | HirType::Null
                    )
                {
                    *ty = inferred;
                }
            }
            Stmt::Return(value) => returned.push(
                value
                    .as_ref()
                    .map_or(HirType::Void, |value| expression(value, env)),
            ),
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                scan(then_branch, env, returned, resolve);
                if let Some(body) = else_branch {
                    scan(body, env, returned, resolve);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                scan(body, env, returned, resolve)
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    scan(std::slice::from_mut(init), env, returned, resolve);
                }
                scan(body, env, returned, resolve);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                scan(body, env, returned, resolve);
                if let Some(catch) = catch {
                    scan(&mut catch.body, env, returned, resolve);
                }
                if let Some(body) = finally {
                    scan(body, env, returned, resolve);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    scan(&mut case.body, env, returned, resolve);
                }
            }
            Stmt::Labeled { body, .. } => scan(std::slice::from_mut(body), env, returned, resolve),
            _ => {}
        }
    }
}

pub(super) fn unresolved_types(hir: &mut HirModule, locations: &crate::locations::Locations<'_>) {
    for _ in 0..=hir.functions.len() {
        let mut changed = false;
        let module_env = perry_hir::HirTypeEnv::from_module(hir);
        for function in &mut hir.functions {
            if locations.explicit_returns.contains(&function.name) {
                continue;
            }
            let mut env = module_env.clone();
            let mut returned = Vec::new();
            scan(&mut function.body, &mut env, &mut returned, false);
            if function.is_async {
                for ty in &mut returned {
                    while let HirType::Promise(inner) = ty {
                        *ty = *inner.clone();
                    }
                }
            }
            if returned
                .iter()
                .any(|ty| matches!(ty, HirType::Any | HirType::Unknown))
            {
                continue;
            }
            if array_methods::returns_optional(&function.body) {
                returned.push(HirType::Void);
            }
            let mut types = Vec::new();
            for ty in returned {
                if !types.contains(&ty) {
                    types.push(ty);
                }
            }
            let ty = match types.as_slice() {
                [] => HirType::Void,
                [ty] => ty.clone(),
                _ if types
                    .iter()
                    .filter(|ty| !matches!(ty, HirType::Null | HirType::Void))
                    .count()
                    == 1 =>
                {
                    HirType::Union(types)
                }
                _ => continue,
            };
            let inferred = if function.is_async {
                HirType::Promise(Box::new(ty))
            } else {
                ty
            };
            if inferred != function.return_type {
                function.return_type = inferred;

                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    let module_env = perry_hir::HirTypeEnv::from_module(hir);
    for function in &mut hir.functions {
        scan(
            &mut function.body,
            &mut module_env.clone(),
            &mut Vec::new(),
            true,
        );
    }
}
