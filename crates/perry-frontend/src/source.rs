//! Source validation, lexical binding facts and expression locations from SWC.
use crate::hir_compat::Locations;
mod collections;
pub(crate) use collections::{CollectionKind, Constructor, Constructors};
mod names;
mod parser;
mod program;
pub(crate) use names::{FreshNames, MODULE_PREFIX, is_reserved_binding, module_binding_name};
pub(crate) use parser::parse;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Result, unsupported};
pub(crate) use program::{
    Bodies, BodyIndex, CallerKind, ModuleDeclarations, SourceModule, authored_body,
};
use std::collections::{HashMap, HashSet};
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

#[derive(Clone, Copy, Default)]
pub(crate) struct SourceBinding {
    inferred_end: Option<u32>,
    pub(crate) deferred: bool,
    pub(crate) loop_let: bool,
    pub(crate) annotated_parameter: bool,
}

impl SourceBinding {
    pub(crate) fn inferred(self, end: u32) -> bool {
        self.inferred_end == Some(end)
    }
}

#[derive(Default)]
pub(crate) struct SourceFacts {
    pub(crate) constructors: Constructors,
    pub(crate) sparse_arrays: bool,
    bindings: HashMap<u32, SourceBinding>,
}

impl SourceFacts {
    pub(crate) fn binding(&self, start: u32) -> SourceBinding {
        self.bindings.get(&start).copied().unwrap_or_default()
    }
}

pub(crate) fn validate_ast(module: &ast::Module, locations: &Locations<'_>) -> Result<()> {
    #[derive(Default)]
    struct SyntaxValidator {
        invalid_surrogate: Option<Span>,
        code_point_arity: Option<Span>,
        enum_decl: Option<Span>,
    }

    impl Visit for SyntaxValidator {
        fn visit_ts_enum_decl(&mut self, decl: &ast::TsEnumDecl) {
            if !decl.declare {
                self.enum_decl.get_or_insert(decl.span);
            }
        }
        fn visit_str(&mut self, string: &ast::Str) {
            if string.value.as_str().is_none() {
                self.invalid_surrogate.get_or_insert(string.span);
            }
        }
        fn visit_tpl_element(&mut self, text: &ast::TplElement) {
            if text.cooked.as_ref().is_some_and(|v| v.as_str().is_none()) {
                self.invalid_surrogate.get_or_insert(text.span);
            }
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if let ast::Callee::Expr(callee) = &call.callee
                && let ast::Expr::Member(member) = callee.as_ref()
                && matches!(member.obj.as_ref(), ast::Expr::Ident(id) if id.sym == "String")
                && matches!(&member.prop, ast::MemberProp::Ident(id) if id.sym == "fromCodePoint")
                && !matches!(call.args.as_slice(), [arg] if arg.spread.is_none())
            {
                self.code_point_arity.get_or_insert(call.span);
            }
            call.visit_children_with(self);
        }
    }

    let mut v = SyntaxValidator::default();
    module.visit_with(&mut v);
    for (hit, msg) in [
        (
            v.invalid_surrogate,
            "lone UTF-16 surrogate in a string literal; Perry strings contain Unicode scalars only",
        ),
        (
            v.code_point_arity,
            "String.fromCodePoint currently requires exactly one scalar argument",
        ),
        (
            v.enum_decl,
            "TypeScript enum declarations are not supported yet; use a union of string or number literals instead",
        ),
    ] {
        if let Some(span) = hit {
            return Err(locations.annotate(unsupported!("{msg}"), span));
        }
    }
    Ok(())
}

#[derive(Default)]
pub(crate) struct Analysis {
    pub(crate) facts: SourceFacts,
    pub(crate) expression_spans: HashSet<Span>,
    pub(crate) names: FreshNames,
}

pub(crate) fn analyze(module: &ast::Module) -> Analysis {
    #[derive(Default)]
    struct AstScanner {
        analysis: Analysis,
        closures: HashSet<ast::Id>,
        enclosing_depth: u32,
    }

    impl AstScanner {
        fn record_param(&mut self, pat: &ast::Pat) {
            walk_pattern(pat, false, &mut |site, annotated| {
                if annotated {
                    let span = match site {
                        BindingSite::Name(id) => id.span,
                        BindingSite::Pattern(span) => span,
                    };
                    self.analysis
                        .facts
                        .bindings
                        .entry(span.lo.0)
                        .or_default()
                        .annotated_parameter = true;
                }
            });
        }
        fn is_closure_expr(&self, expr: &ast::Expr) -> bool {
            match expr {
                ast::Expr::Ident(id) => self.closures.contains(&id.to_id()),
                ast::Expr::Paren(p) => self.is_closure_expr(&p.expr),
                other => matches!(other, ast::Expr::Arrow(_) | ast::Expr::Fn(_)),
            }
        }
    }

    impl Visit for AstScanner {
        fn visit_ident(&mut self, ident: &ast::Ident) {
            self.analysis.names.reserve(&ident.sym);
        }

        fn visit_array_lit(&mut self, array: &ast::ArrayLit) {
            self.analysis.facts.sparse_arrays |= array.elems.iter().any(Option::is_none);
            array.visit_children_with(self);
        }
        fn visit_param(&mut self, param: &ast::Param) {
            self.record_param(&param.pat);
            param.visit_children_with(self);
        }
        fn visit_arrow_expr(&mut self, arrow: &ast::ArrowExpr) {
            for pat in &arrow.params {
                self.record_param(pat);
            }
            self.enclosing_depth += 1;
            arrow.visit_children_with(self);
            self.enclosing_depth -= 1;
        }
        fn visit_fn_decl(&mut self, decl: &ast::FnDecl) {
            if self.enclosing_depth > 0 {
                self.closures.insert(decl.ident.to_id());
            }
            self.enclosing_depth += 1;
            decl.visit_children_with(self);
            self.enclosing_depth -= 1;
        }
        fn visit_for_stmt(&mut self, stmt: &ast::ForStmt) {
            if let Some(ast::VarDeclOrExpr::VarDecl(var)) = &stmt.init
                && var.kind == ast::VarDeclKind::Let
            {
                for decl in &var.decls {
                    pattern_bindings(&decl.name, &mut |id, _| {
                        self.analysis
                            .facts
                            .bindings
                            .entry(id.span.lo.0)
                            .or_default()
                            .loop_let = true;
                    });
                }
            }
            stmt.visit_children_with(self);
        }
        fn visit_for_head(&mut self, head: &ast::ForHead) {
            if let ast::ForHead::VarDecl(var) = head {
                for decl in &var.decls {
                    decl.visit_children_with(self);
                }
            } else {
                head.visit_children_with(self);
            }
        }
        fn visit_var_decl(&mut self, declaration: &ast::VarDecl) {
            let is_const = declaration.kind == ast::VarDeclKind::Const;
            for decl in &declaration.decls {
                if decl.init.is_none()
                    && let ast::Pat::Ident(b) = &decl.name
                {
                    self.analysis
                        .facts
                        .bindings
                        .entry(b.id.span.lo.0)
                        .or_default()
                        .deferred = true;
                }
                pattern_bindings(&decl.name, &mut |id, annotated| {
                    if !annotated {
                        self.analysis
                            .facts
                            .bindings
                            .entry(id.span.lo.0)
                            .or_default()
                            .inferred_end = Some(id.span.hi.0);
                    }
                });
                decl.name.visit_with(self);
                if let Some(init) = &decl.init {
                    if is_const
                        && let ast::Pat::Ident(b) = &decl.name
                        && self.is_closure_expr(init)
                    {
                        self.closures.insert(b.id.to_id());
                    }
                    init.visit_with(self);
                }
            }
        }
        fn visit_cond_expr(&mut self, cond: &ast::CondExpr) {
            self.analysis.expression_spans.insert(cond.span);
            cond.visit_children_with(self);
        }
        fn visit_assign_expr(&mut self, assign: &ast::AssignExpr) {
            if matches!(
                &assign.left,
                ast::AssignTarget::Simple(ast::SimpleAssignTarget::Member(_))
            ) && (self.is_closure_expr(&assign.right)
                || matches!(
                    &*assign.right,
                    ast::Expr::Ident(_) | ast::Expr::Call(_) | ast::Expr::Member(_)
                ))
            {
                self.analysis.expression_spans.insert(assign.span);
            }
            assign.visit_children_with(self);
        }
        fn visit_bin_expr(&mut self, expr: &ast::BinExpr) {
            if matches!(
                expr.op,
                ast::BinaryOp::EqEqEq
                    | ast::BinaryOp::NotEqEq
                    | ast::BinaryOp::EqEq
                    | ast::BinaryOp::NotEq
            ) {
                let is_undef =
                    |e: &ast::Expr| matches!(e, ast::Expr::Ident(id) if id.sym == "undefined");
                if !is_undef(&expr.left)
                    && !is_undef(&expr.right)
                    && (self.is_closure_expr(&expr.left)
                        || self.is_closure_expr(&expr.right)
                        || matches!(
                            (&*expr.left, &*expr.right),
                            (
                                ast::Expr::Ident(_) | ast::Expr::Call(_) | ast::Expr::Member(_),
                                ast::Expr::Ident(_) | ast::Expr::Call(_) | ast::Expr::Member(_)
                            )
                        ))
                {
                    self.analysis.expression_spans.insert(expr.span);
                }
            }
            expr.visit_children_with(self);
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if let ast::Callee::Expr(callee) = &call.callee {
                let is_special_host_call = match callee.as_ref() {
                    ast::Expr::Ident(id) => id.sym == "queueMicrotask",
                    ast::Expr::Member(member) => {
                        matches!(member.obj.as_ref(), ast::Expr::Ident(id) if id.sym == "process")
                            && matches!(&member.prop, ast::MemberProp::Ident(id) if matches!(id.sym.as_ref(), "nextTick" | "on" | "once"))
                    }
                    _ => false,
                };
                if is_special_host_call
                    || call.args.iter().any(|arg| self.is_closure_expr(&arg.expr))
                {
                    self.analysis.expression_spans.insert(call.span);
                }
            }
            call.visit_children_with(self);
        }
    }

    let mut scanner = AstScanner::default();
    module.visit_with(&mut scanner);
    scanner.analysis
}

enum BindingSite<'a> {
    Name(&'a ast::Ident),
    Pattern(Span),
}

pub(crate) fn pattern_bindings<'a>(
    pat: &'a ast::Pat,
    visit: &mut impl FnMut(&'a ast::Ident, bool),
) {
    walk_pattern(pat, false, &mut |site, annotated| {
        if let BindingSite::Name(id) = site {
            visit(id, annotated);
        }
    });
}

fn walk_pattern<'a>(
    pat: &'a ast::Pat,
    inherited: bool,
    visit: &mut impl FnMut(BindingSite<'a>, bool),
) {
    match pat {
        ast::Pat::Ident(binding) => visit(
            BindingSite::Name(&binding.id),
            inherited || binding.type_ann.is_some(),
        ),
        ast::Pat::Array(array) => {
            let annotated = inherited || array.type_ann.is_some();
            visit(BindingSite::Pattern(array.span), annotated);
            for element in array.elems.iter().flatten() {
                walk_pattern(element, annotated, visit);
            }
        }
        ast::Pat::Object(object) => {
            let annotated = inherited || object.type_ann.is_some();
            visit(BindingSite::Pattern(object.span), annotated);
            for prop in &object.props {
                match prop {
                    ast::ObjectPatProp::KeyValue(kv) => walk_pattern(&kv.value, annotated, visit),
                    ast::ObjectPatProp::Assign(assign) => {
                        visit(BindingSite::Name(&assign.key.id), annotated)
                    }
                    ast::ObjectPatProp::Rest(rest) => walk_pattern(&rest.arg, annotated, visit),
                }
            }
        }
        ast::Pat::Rest(rest) => {
            let annotated = inherited || rest.type_ann.is_some();
            visit(BindingSite::Pattern(rest.span), annotated);
            walk_pattern(&rest.arg, annotated, visit);
        }
        ast::Pat::Assign(assign) => walk_pattern(&assign.left, inherited, visit),
        ast::Pat::Invalid(_) | ast::Pat::Expr(_) => {}
    }
}
