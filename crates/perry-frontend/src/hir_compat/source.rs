//! Parse TypeScript with diagnostics before HIR lowering.
use super::*;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::unsupported;
use std::collections::HashSet;
use swc_common::{Span, Spanned};
use swc_ecma_visit::{Visit, VisitWith};

#[derive(Default)]
pub(crate) struct BindingSpans {
    pub inferred: HashSet<(u32, u32)>,
    pub deferred: HashSet<u32>,
}

/// Validate string literals / `String.fromCodePoint` and collect untyped variable bindings in one pass.
pub(crate) fn validate(
    module: &ast::Module,
    locations: &locations::Locations<'_>,
) -> Result<BindingSpans> {
    #[derive(Default)]
    struct Analyzer {
        invalid: Option<Span>,
        code_point_arity: Option<Span>,
        enum_decl: Option<Span>,
        bindings: BindingSpans,
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
        fn visit_for_head(&mut self, head: &ast::ForHead) {
            if let ast::ForHead::VarDecl(var) = head {
                // Iteration initializes the binding. Nested patterns can still contain declarations.
                for declaration in &var.decls {
                    declaration.visit_children_with(self);
                }
            } else {
                head.visit_children_with(self);
            }
        }
        fn visit_var_declarator(&mut self, declaration: &ast::VarDeclarator) {
            if declaration.init.is_none()
                && let ast::Pat::Ident(binding) = &declaration.name
            {
                self.bindings.deferred.insert(binding.id.span.lo.0);
            }
            pattern_bindings(&declaration.name, false, &mut |id, annotated| {
                if !annotated {
                    self.bindings.inferred.insert((id.span.lo.0, id.span.hi.0));
                }
            });
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

/// Bindings and source spans whose identities Perry HIR erases.
#[derive(Default)]
pub(crate) struct CallableBindings {
    pub implicit: HashMap<String, Vec<usize>>,
    pub invoked: HashSet<u32>,
    pub value_uses: HashMap<u32, Span>,
}

pub(super) fn callable_bindings(
    module: &ast::Module,
    locations: &mut locations::Locations<'_>,
) -> Result<CallableBindings> {
    #[derive(Default)]
    struct Uses {
        declarations: HashMap<ast::Id, Vec<usize>>,
        escapes: HashSet<ast::Id>,
        parameters: HashMap<ast::Id, u32>,
        value_uses: HashMap<u32, Span>,
        closures: HashMap<ast::Id, Span>,
        invoked: HashSet<ast::Id>,
        enclosing: Option<swc_atoms::Atom>,
        escaping: Option<(String, Span)>,
        comparisons: HashSet<Span>,
    }
    impl Uses {
        fn parameter(&mut self, pattern: &ast::Pat) {
            if let ast::Pat::Ident(binding) = pattern {
                self.parameters
                    .insert(binding.id.to_id(), binding.id.span.lo.0);
            }
        }
        fn closure(&self, expr: &ast::Expr) -> bool {
            match expr {
                ast::Expr::Ident(id) => self.closures.contains_key(&id.to_id()),
                ast::Expr::Paren(paren) => self.closure(&paren.expr),
                other => matches!(other, ast::Expr::Arrow(_) | ast::Expr::Fn(_)),
            }
        }
        fn invoke(&mut self, expr: &ast::Expr) {
            if let ast::Expr::Ident(id) = expr {
                self.invoked.insert(id.to_id());
            }
        }
        fn callable(&mut self, expr: &ast::Expr) {
            if matches!(expr, ast::Expr::Arrow(_) | ast::Expr::Fn(_)) {
                expr.visit_children_with(self);
                return;
            }
            if let ast::Expr::Paren(paren) = expr {
                self.callable(&paren.expr);
                return;
            }
            if !matches!(expr, ast::Expr::Ident(_)) {
                expr.visit_with(self);
            }
        }
    }
    impl Visit for Uses {
        fn visit_param(&mut self, param: &ast::Param) {
            self.parameter(&param.pat);
            param.visit_children_with(self);
        }
        fn visit_arrow_expr(&mut self, arrow: &ast::ArrowExpr) {
            for pattern in &arrow.params {
                self.parameter(pattern);
            }
            arrow.visit_children_with(self);
        }
        fn visit_fn_decl(&mut self, decl: &ast::FnDecl) {
            if self.enclosing.is_some() {
                self.closures.insert(decl.ident.to_id(), decl.ident.span);
            }
            let enclosing = self.enclosing.replace(decl.ident.sym.clone());
            decl.function.visit_with(self);
            self.enclosing = enclosing;
        }
        fn visit_expr(&mut self, expr: &ast::Expr) {
            if let ast::Expr::Ident(id) = expr
                && let Some(origin) = self.parameters.get(&id.to_id())
            {
                self.value_uses.entry(*origin).or_insert(id.span);
            }
            if self.closure(expr) {
                self.escaping.get_or_insert_with(|| {
                    let name = match expr {
                        ast::Expr::Ident(id) => format!("function '{}'", id.sym),
                        _ => self.enclosing.as_ref().map_or_else(
                            || "anonymous function".into(),
                            |name| format!("closure in '{}'", super::display_name(name)),
                        ),
                    };
                    (name, expr.span())
                });
            } else {
                expr.visit_children_with(self);
            }
        }
        fn visit_ident(&mut self, ident: &ast::Ident) {
            if self.declarations.contains_key(&ident.to_id()) {
                self.escapes.insert(ident.to_id());
            }
        }
        fn visit_var_decl(&mut self, declaration: &ast::VarDecl) {
            for decl in &declaration.decls {
                decl.name.visit_with(self);
                if let Some(init) = &decl.init {
                    if let ast::Pat::Ident(binding) = &decl.name
                        && declaration.kind == ast::VarDeclKind::Const
                        && self.closure(init)
                    {
                        let origin = match init.as_ref() {
                            ast::Expr::Ident(other) => self
                                .closures
                                .get(&other.to_id())
                                .copied()
                                .unwrap_or(binding.id.span),
                            _ => binding.id.span,
                        };
                        self.closures.insert(binding.id.to_id(), origin);
                    }
                    if declaration.kind == ast::VarDeclKind::Const
                        && let (ast::Pat::Ident(binding), ast::Expr::Ident(source)) =
                            (&decl.name, init.as_ref())
                        && let Some(origin) = self.parameters.get(&source.to_id()).copied()
                    {
                        self.parameters.insert(binding.id.to_id(), origin);
                    }
                    self.callable(init);
                }
            }
        }
        fn visit_bin_expr(&mut self, expr: &ast::BinExpr) {
            if matches!(
                expr.op,
                ast::BinaryOp::EqEqEq
                    | ast::BinaryOp::NotEqEq
                    | ast::BinaryOp::EqEq
                    | ast::BinaryOp::NotEq
            ) {
                self.invoke(&expr.left);
                self.invoke(&expr.right);
                if self.closure(&expr.left) || self.closure(&expr.right) {
                    self.comparisons.insert(expr.span);
                }
                self.callable(&expr.left);
                self.callable(&expr.right);
            } else {
                expr.visit_children_with(self);
            }
        }
        fn visit_call_expr(&mut self, call: &ast::CallExpr) {
            if let ast::Callee::Expr(callee) = &call.callee
                && let ast::Expr::Ident(_) = callee.as_ref()
            {
                self.invoke(callee);
                for arg in &call.args {
                    self.invoke(&arg.expr);
                }
            }
            match &call.callee {
                ast::Callee::Expr(expr) => self.callable(expr),
                callee => callee.visit_with(self),
            }
            for arg in &call.args {
                self.callable(&arg.expr);
            }
        }
    }
    let mut uses = Uses::default();
    'functions: for item in &module.body {
        let ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(decl))) = item else {
            continue;
        };
        if decl.function.type_params.is_some() {
            continue;
        }
        let mut indices = Vec::new();
        for (index, param) in decl.function.params.iter().enumerate() {
            let ast::Pat::Ident(binding) = &param.pat else {
                continue 'functions;
            };
            if binding.id.optional {
                continue 'functions;
            }
            if binding.type_ann.is_none() {
                indices.push(index);
            }
        }
        if !indices.is_empty() {
            uses.declarations.insert(decl.ident.to_id(), indices);
        }
    }
    module.visit_with(&mut uses);
    if let Some((name, span)) = uses.escaping {
        return Err(locations.annotate(unsupported!("{name} escapes its static call site. Storing, returning or joining function values requires a runtime function representation"), span));
    }
    locations.callable_comparisons = uses.comparisons;
    let implicit = uses
        .declarations
        .into_iter()
        .filter(|(id, _)| !uses.escapes.contains(id))
        .map(|((name, _), positions)| (name.to_string(), positions))
        .collect();
    let invoked = uses
        .invoked
        .iter()
        .filter_map(|id| uses.closures.get(id).map(|span| span.lo.0))
        .collect();
    Ok(CallableBindings {
        implicit,
        invoked,
        value_uses: uses.value_uses,
    })
}
