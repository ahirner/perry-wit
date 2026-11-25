//! Source validation, lexical binding facts and expression locations from SWC.
use crate::engine::types::LocalFlags;
use crate::hir_compat::Locations;
mod constructors;
pub(crate) use constructors::{
    BuiltinConstructor, CollectionKind, Constructor, ConstructorSites, Constructors,
};
mod names;
mod parser;
mod program;
mod strings;
pub(crate) use names::{FreshNames, MODULE_PREFIX, is_reserved_binding, module_binding_name};
pub(crate) use parser::parse;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Result, unsupported};
pub(crate) use program::{
    Bodies, CallerKind, InstanceBodies, InstanceClass, InstanceFunction, ModuleDeclarations,
    SourceModule, StaticField, authored_body, is_pure_func_expr,
};
use std::collections::{HashMap, HashSet};
pub(crate) use strings::StringConstants;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub(crate) type LiteralClasses<'s> = HashMap<&'s str, &'s [perry_hir::ir::ClassField]>;

#[derive(Clone, Copy, Default)]
struct SourceBinding {
    inferred_end: Option<u32>,
    iteration_end: Option<u32>,
    flags: LocalFlags,
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
    bindings: HashMap<u32, SourceBinding>,
    pub(crate) sparse_arrays: bool,

    pub(crate) names: FreshNames,
}

impl Analysis {
    pub(crate) fn record_iteration(&mut self, span: Span) {
        self.bindings.entry(span.lo.0).or_default().iteration_end = Some(span.hi.0);
    }
}

pub(crate) fn analyze(module: &mut ast::Module, locations: &mut Locations<'_>) -> Analysis {
    struct AstScanner<'a, 's> {
        locations: &'a mut Locations<'s>,
        mark_spans: bool,
        analysis: Analysis,
        closures: HashSet<ast::Id>,
        enclosing_depth: u32,
    }

    impl AstScanner<'_, '_> {
        fn infer_pattern(&mut self, pattern: &ast::Pat) {
            pattern_bindings(pattern, &mut |id, annotated| {
                if !annotated {
                    self.analysis
                        .bindings
                        .entry(id.span.lo.0)
                        .or_default()
                        .inferred_end = Some(id.span.hi.0);
                }
            });
        }

        fn record_param(&mut self, pat: &ast::Pat) {
            walk_pattern(pat, false, &mut |site, annotated| {
                if annotated {
                    let span = match site {
                        BindingSite::Name(id) => id.span,
                        BindingSite::Pattern(span) => span,
                    };
                    self.analysis
                        .bindings
                        .entry(span.lo.0)
                        .or_default()
                        .flags
                        .insert(LocalFlags::PARAM_ANNOTATED);
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
        fn needs_location(&self, expr: &ast::Expr) -> bool {
            use ast::{BinaryOp::*, Expr::*};
            let dynamic = |e: &ast::Expr| matches!(e, Ident(_) | Call(_) | Member(_));
            let undefined = |e: &ast::Expr| matches!(e, Ident(id) if id.sym == "undefined");
            match expr {
                TsNonNull(_) | Cond(_) => true,
                Assign(a) => {
                    matches!(
                        &a.left,
                        ast::AssignTarget::Simple(ast::SimpleAssignTarget::Member(_))
                    ) && (dynamic(&a.right) || self.is_closure_expr(&a.right))
                }
                Bin(b) => match b.op {
                    Exp | BitAnd | BitOr | BitXor | LShift | RShift | ZeroFillRShift => true,
                    EqEqEq | NotEqEq | EqEq | NotEq => {
                        !undefined(&b.left)
                            && !undefined(&b.right)
                            && (self.is_closure_expr(&b.left)
                                || self.is_closure_expr(&b.right)
                                || dynamic(&b.left) && dynamic(&b.right))
                    }
                    _ => false,
                },
                Call(call) if let ast::Callee::Expr(callee) = &call.callee => {
                    matches!(&**callee, Ident(id) if id.sym == "queueMicrotask")
                        || matches!(&**callee, Member(m) if matches!(&*m.obj, Ident(id) if id.sym == "process") &&
                            matches!(&m.prop, ast::MemberProp::Ident(id) if matches!(id.sym.as_ref(), "nextTick" | "on" | "once")))
                        || call.args.iter().any(|arg| self.is_closure_expr(&arg.expr))
                }
                _ => false,
            }
        }
    }

    impl VisitMut for AstScanner<'_, '_> {
        fn visit_mut_ident(&mut self, ident: &mut ast::Ident) {
            self.analysis.names.reserve(&ident.sym);
        }

        fn visit_mut_array_lit(&mut self, array: &mut ast::ArrayLit) {
            self.analysis.sparse_arrays |= array.elems.iter().any(Option::is_none);
            array.visit_mut_children_with(self);
        }
        fn visit_mut_param(&mut self, param: &mut ast::Param) {
            self.record_param(&param.pat);
            param.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ast::ArrowExpr) {
            for pat in &arrow.params {
                self.record_param(pat);
            }
            self.enclosing_depth += 1;
            arrow.visit_mut_children_with(self);
            self.enclosing_depth -= 1;
        }
        fn visit_mut_fn_decl(&mut self, decl: &mut ast::FnDecl) {
            if self.enclosing_depth > 0 {
                self.closures.insert(decl.ident.to_id());
            }
            self.enclosing_depth += 1;
            decl.visit_mut_children_with(self);
            self.enclosing_depth -= 1;
        }
        fn visit_mut_for_stmt(&mut self, stmt: &mut ast::ForStmt) {
            if let Some(ast::VarDeclOrExpr::VarDecl(var)) = &stmt.init
                && var.kind == ast::VarDeclKind::Let
            {
                for decl in &var.decls {
                    pattern_bindings(&decl.name, &mut |id, _| {
                        self.analysis
                            .bindings
                            .entry(id.span.lo.0)
                            .or_default()
                            .flags
                            .insert(LocalFlags::FOR_HEAD);
                    });
                }
            }
            stmt.visit_mut_children_with(self);
        }
        fn visit_mut_for_head(&mut self, head: &mut ast::ForHead) {
            if let ast::ForHead::VarDecl(var) = head {
                for decl in &mut var.decls {
                    self.infer_pattern(&decl.name);
                    decl.visit_mut_children_with(self);
                }
            } else {
                head.visit_mut_children_with(self);
            }
        }
        fn visit_mut_var_decl(&mut self, declaration: &mut ast::VarDecl) {
            let is_const = declaration.kind == ast::VarDeclKind::Const;
            for decl in &mut declaration.decls {
                if decl.init.is_none()
                    && let ast::Pat::Ident(b) = &decl.name
                {
                    self.analysis
                        .bindings
                        .entry(b.id.span.lo.0)
                        .or_default()
                        .flags
                        .insert(LocalFlags::DEFERRED);
                }
                self.infer_pattern(&decl.name);
                decl.name.visit_mut_with(self);
                if let Some(init) = &mut decl.init {
                    if is_const
                        && let ast::Pat::Ident(b) = &decl.name
                        && self.is_closure_expr(init)
                    {
                        self.closures.insert(b.id.to_id());
                    }
                    init.visit_mut_with(self);
                }
            }
        }
        fn visit_mut_ts_enum_decl(&mut self, decl: &mut ast::TsEnumDecl) {
            let previous = std::mem::replace(&mut self.mark_spans, false);
            decl.visit_mut_children_with(self);
            self.mark_spans = previous;
        }

        fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
            let preserve = self.mark_spans && self.needs_location(expr);
            expr.visit_mut_children_with(self);
            if preserve {
                self.locations.preserve_expression(expr);
            }
        }
    }

    let mut scanner = AstScanner {
        locations,
        mark_spans: true,
        analysis: Analysis::default(),
        closures: HashSet::new(),
        enclosing_depth: 0,
    };
    module.visit_mut_with(&mut scanner);
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
