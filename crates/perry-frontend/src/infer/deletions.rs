//! Whether a module deletes array elements, so that its arrays may become sparse.
use super::{Inference, expression};
use crate::{
    hir_compat::Metadata,
    shapes::{Declarations, TypeId, class_functions, generics::Resolver},
};
use perry_hir::{
    HirTypeEnv,
    ir::{Expr, Function, Module, Stmt},
    walker::{stmt_any_expr, walk_expr_children},
};
use pliron::context::Context;
use std::collections::HashMap;

/// Whether `delete` targets an element or property of an array-typed value in `module`.
pub(crate) fn deletes_array_elements(
    ctx: &Context,
    module: &Module,
    metadata: &Metadata<'_>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> bool {
    let decls = Declarations::new(
        &module.interfaces,
        &module.type_aliases,
        &module.imports,
        &module.classes,
        foreign,
    );
    let scan = Scan {
        inference: Inference {
            ctx,
            decls: &decls,
            metadata,
            generic_functions: None,
        },
        classes: &module.classes,
    };
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

struct Scan<'a> {
    inference: Inference<'a>,
    classes: &'a [perry_hir::ir::Class],
}

impl Scan<'_> {
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
        // Every local is already typed in `env`, so the function environment starts empty.
        let mut locals = HashMap::new();
        let ty = expression(
            object,
            &mut self.inference.function(env, self.classes, &mut locals),
        );
        let Inference { ctx, decls, .. } = self.inference;
        let ty = match &ty {
            perry_hir::types::Type::Named(name) => decls.resolve_foreign_type(name).unwrap_or(&ty),
            ty => ty,
        };
        Resolver::new(ctx, decls)
            .flatten(&[TypeId::intern(ctx, ty)], 0)
            .iter()
            .any(|ty| matches!(ty.hir(ctx), perry_hir::types::Type::Array(_)))
    }
}
