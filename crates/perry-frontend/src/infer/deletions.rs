//! Whether a module deletes array elements, so that its arrays may become sparse.
use crate::shapes::{Declarations, TypeId, class_functions, generics::Resolver};
use perry_hir::{
    HirTypeEnv, infer_expr_type,
    ir::{Expr, Function, Module, Stmt},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use std::collections::HashMap;

/// Whether `delete` targets an element or property of an array-typed value in `module`.
pub(crate) fn deletes_array_elements(
    ctx: &Context,
    module: &Module,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> bool {
    let decls = Declarations::new(
        &module.interfaces,
        &module.type_aliases,
        &module.imports,
        &module.classes,
        foreign,
    );
    let scan = Scan { ctx, decls: &decls };
    let function = |function: &Function, env: &HirTypeEnv| {
        scan.statements(&function.body, env)
            || function
                .params
                .iter()
                .filter_map(|param| param.default.as_ref())
                .any(|default| scan.expression(default, env))
    };
    let mut env = HirTypeEnv::from_module(module);
    scan.statements(&module.init, &env)
        || module
            .globals
            .iter()
            .filter_map(|global| global.init.as_ref())
            .any(|init| scan.expression(init, &env))
        || module.functions.iter().any(|f| function(f, &env))
        || module.classes.iter().any(|class| {
            env.set_current_class(Some(class.name.clone()));
            class_functions(class).any(|f| function(f, &env))
                || class
                    .fields
                    .iter()
                    .chain(&class.static_fields)
                    .filter_map(|field| field.init.as_ref())
                    .any(|init| scan.expression(init, &env))
        })
}

struct Scan<'a, 'd> {
    ctx: &'a Context,
    decls: &'a Declarations<'d>,
}

impl Scan<'_, '_> {
    fn statements(&self, body: &[Stmt], env: &HirTypeEnv) -> bool {
        body.iter()
            .any(|stmt| stmt_any_expr(stmt, &mut |expr| self.expression(expr, env)))
    }

    fn expression(&self, expr: &Expr, env: &HirTypeEnv) -> bool {
        let mut found = match expr {
            Expr::Delete(target) => match target.as_ref() {
                Expr::IndexGet { object, .. } | Expr::PropertyGet { object, .. } => {
                    self.is_array(object, env)
                }
                _ => false,
            },
            Expr::Closure { body, .. } => self.statements(body, env),
            _ => false,
        };
        walk_expr_children(expr, &mut |child| {
            found = found || self.expression(child, env);
        });
        found
    }

    fn is_array(&self, object: &Expr, env: &HirTypeEnv) -> bool {
        let ty = infer_expr_type(object, env);
        let ty = match &ty {
            perry_hir::types::Type::Named(name) => {
                self.decls.resolve_foreign_type(name).unwrap_or(&ty)
            }
            ty => ty,
        };
        Resolver::new(self.ctx, self.decls)
            .flatten(&[TypeId::intern(self.ctx, ty)], 0)
            .iter()
            .any(|ty| matches!(ty.hir(self.ctx), perry_hir::types::Type::Array(_)))
    }
}
