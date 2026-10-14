//! Original source coordinates for merged AST and HIR declarations.
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic, SourceSpan};
use std::{collections::HashMap, path::PathBuf};
use swc_common::Span;

#[derive(Clone, Copy)]
struct FunctionInfo {
    span: Span,
    explicit_return: bool,
}

#[derive(Default)]
pub(super) struct Locations<'s> {
    files: Vec<(PathBuf, &'s str, u32)>,
    functions: HashMap<String, FunctionInfo>,
    async_init: bool,
}

impl<'s> Locations<'s> {
    pub(super) fn add(&mut self, file: PathBuf, source: &'s str, offset: u32) {
        self.files.push((file, source, offset));
    }

    pub(super) fn async_init(&self) -> bool {
        self.async_init
    }

    pub(super) fn has_explicit_return(&self, name: &str) -> bool {
        self.functions
            .get(name)
            .is_some_and(|info| info.explicit_return)
    }

    pub(super) fn function_span(&self, name: &str) -> Option<SourceSpan> {
        self.functions
            .get(name)
            .and_then(|info| self.span(info.span))
    }

    pub(super) fn annotate_function(&self, error: anyhow::Error, name: &str) -> anyhow::Error {
        let span = self
            .functions
            .get(name)
            .map_or(swc_common::DUMMY_SP, |info| info.span);
        self.annotate(error, span)
    }

    fn span(&self, span: Span) -> Option<SourceSpan> {
        self.files.iter().find_map(|(file, source, offset)| {
            let start = span.lo.0.checked_sub(offset + 1)? as usize;
            let end = span.hi.0.checked_sub(offset + 1)? as usize;
            (start <= source.len() && end <= source.len())
                .then(|| SourceSpan::new(file.clone(), source, start, end))
        })
    }

    pub(super) fn annotate(&self, error: anyhow::Error, span: Span) -> anyhow::Error {
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

    pub(super) fn collect(&mut self, module: &perry_parser::swc_ecma_ast::Module) {
        use perry_parser::swc_ecma_ast as ast;
        use swc_ecma_visit::{Visit, VisitWith};
        struct Functions<'a, 's>(&'a mut Locations<'s>);
        impl Visit for Functions<'_, '_> {
            fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
                self.0.functions.insert(
                    function.ident.sym.to_string(),
                    FunctionInfo {
                        span: function.function.span,
                        explicit_return: function.function.return_type.is_some(),
                    },
                );
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
