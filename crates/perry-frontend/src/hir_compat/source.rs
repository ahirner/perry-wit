//! Parse TypeScript with diagnostics before HIR lowering.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::unsupported;
use std::collections::HashSet;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

/// Validate string literals / `String.fromCodePoint` and collect untyped variable bindings in one pass.
pub(crate) fn validate(
    module: &ast::Module,
    locations: &locations::Locations<'_>,
) -> Result<HashSet<(u32, u32)>> {
    #[derive(Default)]
    struct Analyzer {
        invalid: Option<Span>,
        code_point_arity: Option<Span>,
        enum_decl: Option<Span>,
        bindings: HashSet<(u32, u32)>,
    }
    impl Visit for Analyzer {
        fn visit_ts_enum_decl(&mut self, decl: &ast::TsEnumDecl) {
            if !decl.declare {
                self.enum_decl.get_or_insert(decl.span);
            }
        }
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
                pattern_bindings(&declaration.name, false, &mut |id, annotated| {
                    if !annotated {
                        self.bindings.insert((id.span.lo.0, id.span.hi.0));
                    }
                });
            }
            declaration.visit_children_with(self);
        }
    }
    let mut analyzer = Analyzer::default();
    module.visit_with(&mut analyzer);
    if let Some(span) = analyzer.invalid {
        return Err(locations.annotate(
            unsupported!(
                "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only"
            ),
            span,
        ));
    }
    if let Some(span) = analyzer.code_point_arity {
        return Err(locations.annotate(
            unsupported!("String.fromCodePoint currently requires exactly one scalar argument"),
            span,
        ));
    }
    if let Some(span) = analyzer.enum_decl {
        return Err(locations.annotate(
            unsupported!(
                "TypeScript enum declarations are not supported yet; use a union of string or number literals instead"
            ),
            span,
        ));
    }
    Ok(analyzer.bindings)
}

/// Visit the declared names and whether a type annotation covers each binding.
pub(crate) fn pattern_bindings<'a>(
    pat: &'a ast::Pat,
    annotated: bool,
    visit: &mut impl FnMut(&'a ast::Ident, bool),
) {
    match pat {
        ast::Pat::Ident(binding) => visit(&binding.id, annotated || binding.type_ann.is_some()),
        ast::Pat::Array(array) => {
            for element in array.elems.iter().flatten() {
                pattern_bindings(element, annotated || array.type_ann.is_some(), visit);
            }
        }
        ast::Pat::Object(object) => {
            let annotated = annotated || object.type_ann.is_some();
            for prop in &object.props {
                match prop {
                    ast::ObjectPatProp::KeyValue(kv) => {
                        pattern_bindings(&kv.value, annotated, visit)
                    }
                    ast::ObjectPatProp::Assign(assign) => visit(&assign.key.id, annotated),
                    ast::ObjectPatProp::Rest(rest) => pattern_bindings(&rest.arg, annotated, visit),
                }
            }
        }
        ast::Pat::Rest(rest) => {
            pattern_bindings(&rest.arg, annotated || rest.type_ann.is_some(), visit)
        }
        ast::Pat::Assign(assign) => pattern_bindings(&assign.left, annotated, visit),
        ast::Pat::Invalid(_) | ast::Pat::Expr(_) => {}
    }
}
