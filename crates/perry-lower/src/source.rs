//! Source diagnostics before HIR can erase unsupported string representations.
use anyhow::{Result, ensure};
use perry_parser::swc_ecma_ast as ast;
use swc_ecma_visit::{Visit, VisitWith};

pub(crate) fn validate(module: &ast::Module) -> Result<()> {
    #[derive(Default)]
    struct Strings {
        invalid: bool,
        code_point_arity: bool,
    }
    impl Visit for Strings {
        fn visit_str(&mut self, string: &ast::Str) {
            self.invalid |= string.value.as_str().is_none();
        }
        fn visit_tpl_element(&mut self, text: &ast::TplElement) {
            self.invalid |= text
                .cooked
                .as_ref()
                .is_some_and(|value| value.as_str().is_none());
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if matches!(&call.callee, ast::Callee::Expr(callee) if matches!(&**callee, ast::Expr::Member(member) if matches!(&*member.obj, ast::Expr::Ident(id) if id.sym == "String") && matches!(&member.prop, ast::MemberProp::Ident(id) if id.sym == "fromCodePoint")))
            {
                self.code_point_arity |=
                    call.args.len() != 1 || call.args.iter().any(|arg| arg.spread.is_some());
            }
            call.visit_children_with(self);
        }
    }
    let mut strings = Strings::default();
    module.visit_with(&mut strings);
    ensure!(
        !strings.invalid,
        "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
    );
    ensure!(
        !strings.code_point_arity,
        "String.fromCodePoint currently requires exactly one scalar argument"
    );
    Ok(())
}

/// Bindings with no declared type may keep the initializer's absence states.
pub(crate) fn inferred_bindings(module: &ast::Module) -> std::collections::HashSet<(u32, u32)> {
    #[derive(Default)]
    struct Bindings(std::collections::HashSet<(u32, u32)>);
    impl Visit for Bindings {
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if let ast::Pat::Ident(binding) = &declaration.name
                && binding.type_ann.is_none()
                && declaration.init.is_some()
            {
                self.0.insert((binding.id.span.lo.0, binding.id.span.hi.0));
            }
            declaration.visit_children_with(self);
        }
    }
    let mut bindings = Bindings::default();
    module.visit_with(&mut bindings);
    bindings.0
}
