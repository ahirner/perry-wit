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
pub(crate) use constructors::RegExpArguments;
pub(crate) use names::{FreshNames, MODULE_PREFIX, is_reserved_binding, module_binding_name};
pub(crate) use parser::parse;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Result, unsupported};
pub(crate) use program::{
    Arguments, Bodies, Callee, CallerKind, FieldOverride, InstanceBodies, InstanceClass,
    InstanceFunction, ModuleDeclarations, SourceCall, SourceModule, authored_body,
};
use std::collections::{HashMap, HashSet};
pub(crate) use strings::ConstantStrings;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ClassId(perry_hir::ir::ClassId);

impl ClassId {
    pub(crate) fn of(class: &perry_hir::ir::Class) -> Option<Self> {
        (!crate::hir_compat::is_object_literal(&class.name)).then_some(Self(class.id))
    }
}

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
    pub(crate) classes: HashSet<ast::Id>,
    pub(crate) sparse_arrays: bool,

    pub(crate) names: FreshNames,
}

impl Analysis {
    pub(crate) fn record_iteration(&mut self, span: Span) {
        self.bindings.entry(span.lo.0).or_default().iteration_end = Some(span.hi.0);
    }
}

pub(crate) fn analyze(
    module: &mut ast::Module,
    locations: &mut Locations<'_>,
    top_level: swc_common::SyntaxContext,
) -> Analysis {
    struct AstScanner<'a, 's> {
        locations: &'a mut Locations<'s>,
        top_level: swc_common::SyntaxContext,
        analysis: Analysis,
    }

    impl AstScanner<'_, '_> {
        fn infer_pattern(&mut self, pattern: &ast::Pat) {
            pattern_bindings(pattern, &mut |id, annotated| {
                let binding = self.analysis.bindings.entry(id.span.lo.0).or_default();
                if !annotated {
                    binding.inferred_end = Some(id.span.hi.0);
                }
                if id.ctxt == self.top_level {
                    binding.flags.insert(LocalFlags::MODULE);
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
    }

    impl VisitMut for AstScanner<'_, '_> {
        fn visit_mut_class_decl(&mut self, decl: &mut ast::ClassDecl) {
            self.analysis.classes.insert(decl.ident.to_id());
            decl.visit_mut_children_with(self);
        }

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
            arrow.visit_mut_children_with(self);
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
                decl.visit_mut_children_with(self);
            }
        }
        fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
            expr.visit_mut_children_with(self);
            self.locations.bind_expression(expr);
        }
    }

    let mut scanner = AstScanner {
        locations,
        top_level,
        analysis: Analysis::default(),
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
