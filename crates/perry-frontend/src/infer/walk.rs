use super::*;
use crate::shapes::{
    TypeId,
    generics::{Declaration, Resolver, substitute_id},
};
use perry_hir::walker::walk_expr_children_mut;

pub(super) enum Node<'a> {
    Binding(&'a mut LocalId, &'a mut HirType),
    Local(&'a mut LocalId),
    Type(&'a mut HirType),
    Expression(&'a mut Expr),
}

pub(super) fn visit_expr<V: Visitor>(expr: &mut Expr, visit: &mut V) -> Result<(), V::Error> {
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

pub(super) fn visit_body<V: Visitor>(
    statements: &mut [Stmt],
    visit: &mut V,
) -> Result<(), V::Error> {
    for statement in statements {
        visit.statement(statement)?;
    }
    Ok(())
}

pub(super) fn visit_stmt<V: Visitor>(statement: &mut Stmt, visit: &mut V) -> Result<(), V::Error> {
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
    Ok(())
}

pub(super) trait Visitor: Sized {
    type Error;

    fn node(&mut self, node: Node<'_>) -> Result<(), Self::Error>;

    fn statement(&mut self, statement: &mut Stmt) -> Result<(), Self::Error> {
        visit_stmt(statement, self)
    }

    fn expression(&mut self, expr: &mut Expr) -> Result<(), Self::Error> {
        visit_expr(expr, self)
    }
}

impl<F: FnMut(Node<'_>) -> Result<()>> Visitor for F {
    type Error = anyhow::Error;

    fn node(&mut self, node: Node<'_>) -> Result<()> {
        self(node)
    }
}

pub(super) fn function<V: Visitor>(
    function: &mut perry_hir::ir::Function,
    visit: &mut V,
) -> Result<(), V::Error> {
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
        type Error = std::convert::Infallible;

        fn node(&mut self, node: Node<'_>) -> Result<(), Self::Error> {
            if let Node::Binding(id, ty) = node {
                self.0(*id, ty);
            }
            Ok(())
        }
        fn expression(&mut self, _: &mut Expr) -> Result<(), Self::Error> {
            Ok(())
        }
    }
    let Ok(()) = visit_body(statements, &mut Locals(visit));
}

pub(super) fn resolve_locals(statements: &mut [Stmt], mut env: FunctionEnv<'_>) {
    let mut bindings = HashMap::new();
    scan(statements, &mut env, None, Some(&mut bindings));
    locals_mut(statements, &mut |id, ty| {
        if let Some(inferred) = bindings.remove(&id) {
            *ty = inferred;
        }
    });
}

fn reader_kind(ty: &HirType, env: &FunctionEnv<'_>) -> Option<Kind> {
    let mut ty = TypeId::intern(env.ctx, ty);
    for _ in 0..128 {
        let (base, arguments) = match ty.hir(env.ctx) {
            HirType::Named(name) => {
                if name
                    .strip_prefix("perry:types/")
                    .and_then(shapes::perry_integer_spec)
                    .is_some()
                {
                    return None;
                }
                if !env.decls.is_user_type(name) {
                    if env.decls.integer_imports.contains_key(name.as_str()) {
                        return None;
                    }
                    let imported = env.decls.imported_types.get(name.as_str()).copied();
                    if imported.is_none()
                        && let Some(kind) = Kind::builtin(name)
                    {
                        return matches!(kind, Kind::Reader | Kind::ByobReader).then_some(kind);
                    }
                    let mut entry = imported.or_else(|| env.decls.foreign.get(name))?;
                    while let crate::hir::Foreign::Input(inner) = entry {
                        entry = inner;
                    }
                    let crate::hir::Foreign::Type(target) = entry else {
                        return None;
                    };
                    ty = TypeId::intern(env.ctx, target);
                    continue;
                }
                (name.as_str(), &[][..])
            }
            HirType::Generic { base, type_args } => {
                if base == "WitInput" || (base == "Readonly" && !env.decls.is_user_type(base)) {
                    let [inner] = type_args.as_slice() else {
                        return None;
                    };
                    ty = TypeId::intern(env.ctx, inner);
                    continue;
                }
                (base.as_str(), type_args.as_slice())
            }
            _ => return None,
        };
        let arguments = TypeId::intern_slice(env.ctx, arguments);
        if !env.decls.is_user_type(base) && matches!(base, "Exclude" | "Extract" | "NonNullable") {
            ty = Resolver::new(env.ctx, env.decls).utility_type(base, &arguments, 0)?;
            continue;
        }
        if env.decls.interfaces.contains_key(base)
            || env.classes.iter().any(|class| class.name == base)
        {
            return None;
        }
        let (_, alias) = env.decls.type_aliases.get(base)?;
        let bindings = Declaration::Alias(alias)
            .bind(env.ctx, base, &arguments)
            .ok()?;
        let mut remaining = usize::MAX;
        ty = substitute_id(env.ctx, &alias.ty, &bindings, &mut remaining).ok()?;
    }
    None
}

pub(super) fn scan(
    statements: &[Stmt],
    env: &mut FunctionEnv<'_>,
    mut returned: Option<&mut Vec<HirType>>,
    mut bindings: Option<&mut HashMap<LocalId, HirType>>,
) {
    for statement in statements {
        match statement {
            Stmt::Let {
                id,
                ty,
                init: Some(init),
                ..
            } if (matches!(ty, HirType::Any | HirType::Unknown)
                || (ty.any(&mut |ty| matches!(ty, HirType::Any))
                    && (matches!(init, Expr::LocalGet(_))
                        || matches!(init, Expr::Call { callee, .. } if matches!(callee.as_ref(), Expr::FuncRef(_)))))
                || (env.metadata.inferred_bindings.contains(id)
                    && matches!(expression(init, env), HirType::Union(_)))
                || matches!(
                    ty,
                    HirType::Generic { base, type_args }
                        if matches!(base.as_str(), "Map" | "Set") && type_args.is_empty()
                )
                || (env.metadata.inferred_bindings.contains(id)
                    && reader_kind(ty, env) == Some(Kind::Reader)))
                && !matches!(init, Expr::Undefined) =>
            {
                let inferred = expression(init, env);
                if reader_kind(ty, env) == Some(Kind::Reader)
                    && reader_kind(&inferred, env) != Some(Kind::ByobReader)
                {
                    continue;
                }
                if let Some(bindings) = bindings.as_deref_mut()
                    && is_resolvable_type(&inferred)
                {
                    bindings.insert(*id, inferred.clone());
                }
                if !matches!(inferred, HirType::Any | HirType::Unknown) {
                    env.locals.insert(*id, inferred);
                }
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
            Stmt::Return(value) => {
                if let Some(returned) = returned.as_deref_mut() {
                    returned.push(
                        value
                            .as_ref()
                            .map_or(HirType::Void, |value| expression(value, env)),
                    );
                }
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                scan(
                    then_branch,
                    env,
                    returned.as_deref_mut(),
                    bindings.as_deref_mut(),
                );
                if let Some(body) = else_branch {
                    scan(body, env, returned.as_deref_mut(), bindings.as_deref_mut());
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                scan(body, env, returned.as_deref_mut(), bindings.as_deref_mut());
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    scan(
                        std::slice::from_ref(init),
                        env,
                        returned.as_deref_mut(),
                        bindings.as_deref_mut(),
                    );
                }
                scan(body, env, returned.as_deref_mut(), bindings.as_deref_mut());
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                scan(body, env, returned.as_deref_mut(), bindings.as_deref_mut());
                if let Some(catch) = catch {
                    scan(
                        &catch.body,
                        env,
                        returned.as_deref_mut(),
                        bindings.as_deref_mut(),
                    );
                }
                if let Some(body) = finally {
                    scan(body, env, returned.as_deref_mut(), bindings.as_deref_mut());
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    scan(
                        &case.body,
                        env,
                        returned.as_deref_mut(),
                        bindings.as_deref_mut(),
                    );
                }
            }
            Stmt::Labeled { body, .. } => {
                scan(
                    std::slice::from_ref(body),
                    env,
                    returned.as_deref_mut(),
                    bindings.as_deref_mut(),
                );
            }
            _ => {}
        }
    }
}
