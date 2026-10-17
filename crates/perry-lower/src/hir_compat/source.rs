//! Source diagnostics before HIR can erase unsupported string representations.
use crate::hir_compat::locations::Locations;
use anyhow::{Result, anyhow};
use perry_parser::swc_ecma_ast as ast;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

pub(crate) fn validate(module: &ast::Module, locations: &Locations<'_>) -> Result<()> {
    #[derive(Default)]
    struct Strings {
        invalid: Option<Span>,
        code_point_arity: Option<Span>,
    }
    impl Visit for Strings {
        fn visit_str(&mut self, string: &ast::Str) {
            if string.value.as_str().is_none() {
                self.invalid.get_or_insert(string.span);
            }
        }
        fn visit_tpl_element(&mut self, text: &ast::TplElement) {
            if text
                .cooked
                .as_ref()
                .is_some_and(|value| value.as_str().is_none())
            {
                self.invalid.get_or_insert(text.span);
            }
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if matches!(&call.callee, ast::Callee::Expr(callee) if matches!(&**callee, ast::Expr::Member(member) if matches!(&*member.obj, ast::Expr::Ident(id) if id.sym == "String") && matches!(&member.prop, ast::MemberProp::Ident(id) if id.sym == "fromCodePoint")))
                && (call.args.len() != 1 || call.args.iter().any(|arg| arg.spread.is_some()))
            {
                self.code_point_arity.get_or_insert(call.span);
            }
            call.visit_children_with(self);
        }
    }
    let mut strings = Strings::default();
    module.visit_with(&mut strings);
    if let Some(span) = strings.invalid {
        return Err(locations.annotate(
            anyhow!(
                "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
            ),
            span,
        ));
    }
    if let Some(span) = strings.code_point_arity {
        return Err(locations.annotate(
            anyhow!("String.fromCodePoint currently requires exactly one scalar argument"),
            span,
        ));
    }
    Ok(())
}

/// Bindings with no declared type may keep the initializer's absence states.
pub(super) fn inferred_bindings(module: &ast::Module) -> std::collections::HashSet<(u32, u32)> {
    fn collect_pat(pat: &ast::Pat, out: &mut std::collections::HashSet<(u32, u32)>) {
        match pat {
            ast::Pat::Ident(binding) if binding.type_ann.is_none() => {
                out.insert((binding.id.span.lo.0, binding.id.span.hi.0));
            }
            ast::Pat::Array(arr) if arr.type_ann.is_none() => {
                for elem in arr.elems.iter().flatten() {
                    collect_pat(elem, out);
                }
            }
            ast::Pat::Object(obj) if obj.type_ann.is_none() => {
                for prop in &obj.props {
                    match prop {
                        ast::ObjectPatProp::KeyValue(kv) => collect_pat(&kv.value, out),
                        ast::ObjectPatProp::Assign(assign) => {
                            out.insert((assign.key.span.lo.0, assign.key.span.hi.0));
                        }
                        ast::ObjectPatProp::Rest(rest) => collect_pat(&rest.arg, out),
                    }
                }
            }
            ast::Pat::Rest(rest) if rest.type_ann.is_none() => collect_pat(&rest.arg, out),
            ast::Pat::Assign(assign) => collect_pat(&assign.left, out),
            _ => {}
        }
    }

    #[derive(Default)]
    struct Bindings(std::collections::HashSet<(u32, u32)>);
    impl Visit for Bindings {
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if declaration.init.is_some() {
                collect_pat(&declaration.name, &mut self.0);
            }
            declaration.visit_children_with(self);
        }
    }
    let mut bindings = Bindings::default();
    module.visit_with(&mut bindings);
    bindings.0
}
