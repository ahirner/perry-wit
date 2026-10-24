//! Parse TypeScript with diagnostics before HIR lowering.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use std::collections::HashSet;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

/// Validate string literals / `String.fromCodePoint` and collect untyped variable bindings in one pass.
pub fn validate(
    module: &ast::Module,
    locations: &locations::Locations<'_>,
) -> Result<HashSet<(u32, u32)>> {
    #[derive(Default)]
    struct Analyzer {
        invalid: Option<Span>,
        code_point_arity: Option<Span>,
        bindings: HashSet<(u32, u32)>,
    }
    impl Visit for Analyzer {
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
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if declaration.init.is_some() {
                collect_pat(&declaration.name, &mut self.bindings);
            }
            declaration.visit_children_with(self);
        }
    }
    let mut analyzer = Analyzer::default();
    module.visit_with(&mut analyzer);
    if let Some(span) = analyzer.invalid {
        return Err(locations.annotate(
            anyhow!(
                "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
            ),
            span,
        ));
    }
    if let Some(span) = analyzer.code_point_arity {
        return Err(locations.annotate(
            anyhow!("String.fromCodePoint currently requires exactly one scalar argument"),
            span,
        ));
    }
    Ok(analyzer.bindings)
}

fn collect_pat(pat: &ast::Pat, out: &mut HashSet<(u32, u32)>) {
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
