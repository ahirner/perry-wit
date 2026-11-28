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

/// Lexical facts about one binding name or pattern, keyed by its start offset.
/// The scanner records a site only when it learns a fact there. A declaration without a record,
/// such as an unannotated parameter, a `catch` binding or a nested function name, has no facts.
#[derive(Clone, Copy, Default)]
struct SourceBinding {
    inferred_end: Option<u32>,
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
    arrays: HashSet<ast::Id>,
    pub(crate) sparse_arrays: bool,

    pub(crate) names: FreshNames,
}

impl Analysis {
    fn binding(&mut self, span: Span) -> &mut SourceBinding {
        self.bindings.entry(span.lo.0).or_default()
    }

    pub(crate) fn array_source(&self, source: &ast::Expr) -> bool {
        match source {
            ast::Expr::Array(array) => !array.elems.is_empty(),
            ast::Expr::Ident(id) => self.arrays.contains(&id.to_id()),
            ast::Expr::Paren(paren) => self.array_source(&paren.expr),
            _ => false,
        }
    }

    pub(crate) fn record_iteration(&mut self, span: Span) {
        let binding = self.binding(span);
        binding.flags.insert(LocalFlags::FOR_OF);
        binding.inferred_end = Some(span.hi.0);
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
        fn record_pattern(&mut self, pat: &ast::Pat, parameter: bool) {
            walk_pattern(pat, false, &mut |site, annotated| {
                let (span, name) = match site {
                    BindingSite::Name(name) => (name.span, Some(name)),
                    BindingSite::Pattern(span) => (span, None),
                };
                if let Some(name) = name
                    && !name.optional
                    && let Some(ann) = &name.type_ann
                    && ann.type_ann.is_ts_array_type()
                {
                    self.analysis.arrays.insert(name.to_id());
                }
                if parameter {
                    if annotated {
                        self.analysis
                            .binding(span)
                            .flags
                            .insert(LocalFlags::PARAM_ANNOTATED);
                    }
                } else if let Some(name) = name {
                    let binding = self.analysis.binding(span);
                    if !annotated {
                        binding.inferred_end = Some(span.hi.0);
                    }
                    if name.ctxt == self.top_level {
                        binding.flags.insert(LocalFlags::MODULE);
                    }
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
            self.record_pattern(&param.pat, true);
            param.visit_mut_children_with(self);
        }
        fn visit_mut_arrow_expr(&mut self, arrow: &mut ast::ArrowExpr) {
            for pat in &arrow.params {
                self.record_pattern(pat, true);
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
                            .binding(id.span)
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
                    self.record_pattern(&decl.name, false);
                    decl.visit_mut_children_with(self);
                }
            } else {
                head.visit_mut_children_with(self);
            }
        }
        fn visit_mut_var_decl(&mut self, declaration: &mut ast::VarDecl) {
            for decl in &mut declaration.decls {
                if let ast::Pat::Ident(name) = &decl.name {
                    match &decl.init {
                        None => self
                            .analysis
                            .binding(name.span)
                            .flags
                            .insert(LocalFlags::DEFERRED),
                        Some(init)
                            if declaration.kind == ast::VarDeclKind::Const
                                && name.type_ann.is_none()
                                && self.analysis.array_source(init) =>
                        {
                            self.analysis.arrays.insert(name.to_id());
                        }
                        _ => {}
                    }
                }
                self.record_pattern(&decl.name, false);
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
    Name(&'a ast::BindingIdent),
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
            BindingSite::Name(binding),
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
                        visit(BindingSite::Name(&assign.key), annotated)
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
