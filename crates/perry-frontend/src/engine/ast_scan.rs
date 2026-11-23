//! Single-pass SWC AST validation and source-fact extraction before HIR lowering.
use crate::hir_compat::{self, Locations};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Result, unsupported};
use std::collections::HashMap;
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitWith};

#[derive(Default)]
pub(crate) struct SourceFacts {
    pub(crate) sparse_arrays: bool,
    pub(crate) inferred_spans: Vec<(u32, u32)>,
    pub(crate) deferred_starts: Vec<u32>,
    pub(crate) for_head_let_starts: Vec<u32>,
    pub(crate) param_annotated_starts: Vec<u32>,
    pub(crate) exported_var_starts: Vec<u32>,
    pub(crate) callable_comparisons: Vec<Span>,
    pub(crate) conditional_spans: Vec<Span>,
    pub(crate) param_value_uses: Vec<(u32, Span)>,
}

impl SourceFacts {
    pub(crate) fn is_inferred(&self, start: u32, end: u32) -> bool {
        self.inferred_spans.binary_search(&(start, end)).is_ok()
    }
    pub(crate) fn is_deferred(&self, start: u32) -> bool {
        self.deferred_starts.binary_search(&start).is_ok()
    }
    pub(crate) fn is_for_head_let(&self, start: u32) -> bool {
        self.for_head_let_starts.binary_search(&start).is_ok()
    }
    pub(crate) fn is_param_annotated(&self, start: u32) -> bool {
        self.param_annotated_starts.binary_search(&start).is_ok()
    }
    pub(crate) fn is_exported_var(&self, start: u32) -> bool {
        self.exported_var_starts.binary_search(&start).is_ok()
    }
    pub(crate) fn param_value_use(&self, start: u32) -> Option<Span> {
        let idx = self
            .param_value_uses
            .binary_search_by_key(&start, |(s, _)| *s)
            .ok()?;
        self.param_value_uses.get(idx).map(|&(_, span)| span)
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

pub(crate) fn scan_ast(module: &ast::Module) -> Result<SourceFacts> {
    #[derive(Default)]
    struct AstScanner {
        facts: SourceFacts,
        parameters: HashMap<ast::Id, u32>,
        closures: HashMap<ast::Id, Span>,
        top_vars: HashMap<ast::Id, u32>,
        enclosing_depth: u32,
    }

    impl AstScanner {
        fn record_param(&mut self, pat: &ast::Pat) {
            record_pattern_annotations(pat, false, &mut self.facts.param_annotated_starts);
            if let ast::Pat::Ident(binding) = pat {
                self.parameters
                    .insert(binding.id.to_id(), binding.id.span.lo.0);
            }
        }
        fn is_closure_expr(&self, expr: &ast::Expr) -> bool {
            match expr {
                ast::Expr::Ident(id) => self.closures.contains_key(&id.to_id()),
                ast::Expr::Paren(p) => self.is_closure_expr(&p.expr),
                other => matches!(other, ast::Expr::Arrow(_) | ast::Expr::Fn(_)),
            }
        }
        fn visit_callable_position(&mut self, expr: &ast::Expr) {
            match expr {
                ast::Expr::Arrow(_) | ast::Expr::Fn(_) => expr.visit_children_with(self),
                ast::Expr::Paren(p) => self.visit_callable_position(&p.expr),
                ast::Expr::Ident(_) => {}
                _ => expr.visit_with(self),
            }
        }
    }

    impl Visit for AstScanner {
        fn visit_export_decl(&mut self, export: &ast::ExportDecl) {
            if let ast::Decl::Var(var) = &export.decl {
                for decl in &var.decls {
                    hir_compat::pattern_bindings(&decl.name, false, &mut |id, _| {
                        self.facts.exported_var_starts.push(id.span.lo.0);
                    });
                }
            }
            export.visit_children_with(self);
        }
        fn visit_named_export(&mut self, export: &ast::NamedExport) {
            if export.src.is_none() {
                for spec in &export.specifiers {
                    if let ast::ExportSpecifier::Named(named) = spec
                        && let ast::ModuleExportName::Ident(ident) = &named.orig
                        && let Some(&start) = self.top_vars.get(&ident.to_id())
                    {
                        self.facts.exported_var_starts.push(start);
                    }
                }
            }
            export.visit_children_with(self);
        }
        fn visit_array_lit(&mut self, array: &ast::ArrayLit) {
            self.facts.sparse_arrays |= array.elems.iter().any(Option::is_none);
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
                self.closures.insert(decl.ident.to_id(), decl.ident.span);
            }
            self.enclosing_depth += 1;
            decl.function.visit_with(self);
            self.enclosing_depth -= 1;
        }
        fn visit_for_stmt(&mut self, stmt: &ast::ForStmt) {
            if let Some(ast::VarDeclOrExpr::VarDecl(var)) = &stmt.init
                && var.kind == ast::VarDeclKind::Let
            {
                for decl in &var.decls {
                    hir_compat::pattern_bindings(&decl.name, false, &mut |id, _| {
                        self.facts.for_head_let_starts.push(id.span.lo.0);
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
                    self.facts.deferred_starts.push(b.id.span.lo.0);
                }
                hir_compat::pattern_bindings(&decl.name, false, &mut |id, annotated| {
                    if self.enclosing_depth == 0 {
                        self.top_vars.insert(id.to_id(), id.span.lo.0);
                    }
                    if !annotated {
                        self.facts.inferred_spans.push((id.span.lo.0, id.span.hi.0));
                    }
                });
                decl.name.visit_with(self);
                if let Some(init) = &decl.init {
                    if is_const && let ast::Pat::Ident(b) = &decl.name {
                        if self.is_closure_expr(init) {
                            let origin = match init.as_ref() {
                                ast::Expr::Ident(o) => {
                                    self.closures.get(&o.to_id()).copied().unwrap_or(b.id.span)
                                }
                                _ => b.id.span,
                            };
                            self.closures.insert(b.id.to_id(), origin);
                        }
                        if let ast::Expr::Ident(src) = init.as_ref()
                            && let Some(&origin) = self.parameters.get(&src.to_id())
                        {
                            self.parameters.insert(b.id.to_id(), origin);
                        }
                    }
                    self.visit_callable_position(init);
                }
            }
        }
        fn visit_expr(&mut self, expr: &ast::Expr) {
            if let ast::Expr::Ident(id) = expr
                && let Some(&origin) = self.parameters.get(&id.to_id())
            {
                self.facts.param_value_uses.push((origin, id.span));
            }
            expr.visit_children_with(self);
        }
        fn visit_cond_expr(&mut self, cond: &ast::CondExpr) {
            self.facts.conditional_spans.push(cond.span);
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
                self.facts.conditional_spans.push(assign.span);
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
                    self.facts.callable_comparisons.push(expr.span);
                }
                self.visit_callable_position(&expr.left);
                self.visit_callable_position(&expr.right);
            } else {
                expr.visit_children_with(self);
            }
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
                if is_special_host_call {
                    self.facts.conditional_spans.push(call.span);
                }
            }
            match &call.callee {
                ast::Callee::Expr(e) => self.visit_callable_position(e),
                callee => callee.visit_with(self),
            }
            for arg in &call.args {
                self.visit_callable_position(&arg.expr);
            }
        }
    }

    let mut scanner = AstScanner::default();
    module.visit_with(&mut scanner);
    scanner.facts.inferred_spans.sort_unstable();
    scanner.facts.inferred_spans.dedup();
    scanner.facts.deferred_starts.sort_unstable();
    scanner.facts.deferred_starts.dedup();
    scanner.facts.for_head_let_starts.sort_unstable();
    scanner.facts.for_head_let_starts.dedup();
    scanner.facts.param_annotated_starts.sort_unstable();
    scanner.facts.param_annotated_starts.dedup();
    scanner.facts.exported_var_starts.sort_unstable();
    scanner.facts.exported_var_starts.dedup();
    scanner
        .facts
        .param_value_uses
        .sort_unstable_by_key(|(s, _)| *s);
    scanner.facts.param_value_uses.dedup_by_key(|(s, _)| *s);
    Ok(scanner.facts)
}

fn record_pattern_annotations(pat: &ast::Pat, inherited: bool, out: &mut Vec<u32>) {
    match pat {
        ast::Pat::Ident(b) if inherited || b.type_ann.is_some() => out.push(b.id.span.lo.0),
        ast::Pat::Array(arr) => {
            let ann = inherited || arr.type_ann.is_some();
            if ann {
                out.push(arr.span.lo.0);
            }
            for elem in arr.elems.iter().flatten() {
                record_pattern_annotations(elem, ann, out);
            }
        }
        ast::Pat::Object(obj) => {
            let ann = inherited || obj.type_ann.is_some();
            if ann {
                out.push(obj.span.lo.0);
            }
            for prop in &obj.props {
                match prop {
                    ast::ObjectPatProp::KeyValue(kv) => {
                        record_pattern_annotations(&kv.value, ann, out)
                    }
                    ast::ObjectPatProp::Assign(a) if ann => out.push(a.key.id.span.lo.0),
                    ast::ObjectPatProp::Rest(r) => record_pattern_annotations(&r.arg, ann, out),
                    ast::ObjectPatProp::Assign(_) => {}
                }
            }
        }
        ast::Pat::Rest(r) => {
            let ann = inherited || r.type_ann.is_some();
            if ann {
                out.push(r.span.lo.0);
            }
            record_pattern_annotations(&r.arg, ann, out);
        }
        ast::Pat::Assign(a) => record_pattern_annotations(&a.left, inherited, out),
        _ => {}
    }
}
