//! Original source coordinates for merged AST and HIR declarations.
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic, SourceSpan};
use std::{collections::HashMap, path::PathBuf};
use swc_common::Span;

#[derive(Default)]
pub(crate) struct Locations<'s> {
    files: Vec<(PathBuf, &'s str, u32)>,
    pub(crate) functions: HashMap<String, Span>,
    pub(crate) explicit_returns: std::collections::HashSet<String>,
    pub(crate) async_init: bool,
}
impl<'s> Locations<'s> {
    pub(crate) fn add(&mut self, file: PathBuf, source: &'s str, offset: u32) {
        self.files.push((file, source, offset));
    }
    pub(crate) fn span(&self, span: Span) -> Option<SourceSpan> {
        self.files.iter().find_map(|(file, source, offset)| {
            let start = span.lo.0.checked_sub(offset + 1)? as usize;
            let end = span.hi.0.checked_sub(offset + 1)? as usize;
            (start <= source.len() && end <= source.len())
                .then(|| SourceSpan::new(file.clone(), source, start, end))
        })
    }
    pub(crate) fn annotate(&self, error: anyhow::Error, span: Span) -> anyhow::Error {
        let mut diagnostic = error
            .downcast_ref::<PerryDiagnostic>()
            .cloned()
            .unwrap_or_else(|| {
                PerryDiagnostic::error(DiagnosticCode::UnsupportedExpression, error.to_string())
            });
        if diagnostic.span.is_none() {
            diagnostic.span = self.span(span);
            error.context(diagnostic)
        } else {
            error
        }
    }
    pub(crate) fn collect(&mut self, module: &perry_parser::swc_ecma_ast::Module) {
        use perry_parser::swc_ecma_ast as ast;
        use swc_ecma_visit::{Visit, VisitWith};
        struct Functions<'a, 's>(&'a mut Locations<'s>);
        impl Visit for Functions<'_, '_> {
            fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
                self.0
                    .functions
                    .insert(function.ident.sym.to_string(), function.function.span);
                if function.function.return_type.is_some() {
                    self.0
                        .explicit_returns
                        .insert(function.ident.sym.to_string());
                }
                function.visit_children_with(self);
            }
        }
        module.visit_with(&mut Functions(self));
        struct TopLevelAwait(bool);
        impl Visit for TopLevelAwait {
            fn visit_await_expr(&mut self, _: &ast::AwaitExpr) {
                self.0 = true;
            }
            fn visit_function(&mut self, _: &ast::Function) {}
            fn visit_arrow_expr(&mut self, _: &ast::ArrowExpr) {}
        }
        let mut awaits = TopLevelAwait(false);
        module.visit_with(&mut awaits);
        self.async_init |= awaits.0;
    }
}
