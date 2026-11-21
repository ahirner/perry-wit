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
                collect_pat(&declaration.name, &mut self.bindings);
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

pub(crate) type PrivateFunctions = HashMap<swc_atoms::Atom, PrivateParameters>;

pub(crate) struct PrivateParameters {
    scope: swc_common::SyntaxContext,
    pub indices: Vec<usize>,
    pub references: HashSet<u32>,
}

/// Record the source position of every resolved reference, including shorthand properties and callbacks.
pub(super) fn private_parameters(module: &ast::Module) -> PrivateFunctions {
    struct Uses(PrivateFunctions);
    impl Visit for Uses {
        fn visit_fn_decl(&mut self, decl: &ast::FnDecl) {
            decl.function.visit_with(self);
        }
        fn visit_ident(&mut self, ident: &ast::Ident) {
            if let Some(function) = self.0.get_mut(&ident.sym)
                && function.scope == ident.ctxt
            {
                function.references.insert(ident.span.lo.0);
            }
        }
    }
    let mut uses = Uses(HashMap::new());
    for item in &module.body {
        let ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(decl))) = item else {
            continue;
        };
        if decl.function.type_params.is_some() {
            continue;
        }
        let mut indices = Vec::new();
        if !decl
            .function
            .params
            .iter()
            .all(|p| matches!(&p.pat, ast::Pat::Ident(binding) if !binding.id.optional))
        {
            continue;
        }
        for (index, param) in decl.function.params.iter().enumerate() {
            if matches!(&param.pat, ast::Pat::Ident(binding) if binding.type_ann.is_none()) {
                indices.push(index);
            }
        }
        if !indices.is_empty() {
            uses.0.insert(
                decl.ident.sym.clone(),
                PrivateParameters {
                    scope: decl.ident.ctxt,
                    indices,
                    references: HashSet::new(),
                },
            );
        }
    }
    if !uses.0.is_empty() {
        module.visit_with(&mut uses);
    }
    uses.0.retain(|_, entry| !entry.references.is_empty());
    uses.0
}
