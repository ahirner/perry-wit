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
    calls: HashMap<u32, Span>,
    types: HashMap<String, Span>,
    imports: HashMap<String, Span>,
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

    pub(super) fn annotate_call(&self, error: anyhow::Error, byte_offset: u32) -> anyhow::Error {
        if byte_offset == 0 {
            return error;
        }
        let span = self.calls.get(&byte_offset).copied().unwrap_or_else(|| {
            Span::new(
                swc_common::BytePos(byte_offset),
                swc_common::BytePos(byte_offset.saturating_add(1)),
            )
        });
        self.annotate(error, span)
    }

    pub(super) fn annotate_type(&self, error: anyhow::Error, name: &str) -> anyhow::Error {
        match self.types.get(name) {
            Some(&span) => self.annotate(error, span),
            None => error,
        }
    }

    pub(super) fn annotate_import(&self, error: anyhow::Error, source: &str) -> anyhow::Error {
        match self.imports.get(source) {
            Some(&span) => self.annotate(error, span),
            None => error,
        }
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
        let existing = error.downcast_ref::<PerryDiagnostic>().cloned();
        if existing.as_ref().is_some_and(|d| d.span.is_some()) {
            return error;
        }
        let resolved = self.span(span);
        if resolved.is_none() && existing.is_some() {
            return error;
        }
        let mut diagnostic = existing.unwrap_or_else(|| {
            PerryDiagnostic::error(DiagnosticCode::UnsupportedExpression, error.to_string())
        });
        diagnostic.span = resolved;
        error.context(diagnostic)
    }

    pub(super) fn collect(&mut self, module: &perry_parser::swc_ecma_ast::Module) {
        use perry_parser::swc_ecma_ast as ast;
        use swc_ecma_visit::{Visit, VisitWith};
        struct Collector<'a, 's>(&'a mut Locations<'s>);
        impl Visit for Collector<'_, '_> {
            fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
                self.0.functions.insert(
                    function.ident.sym.to_string(),
                    FunctionInfo {
                        span: function.function.span,
                        explicit_return: function.function.return_type.is_some(),
                    },
                );
                function.visit_children_with(self);
            }
            fn visit_var_declarator(&mut self, decl: &ast::VarDeclarator) {
                if let (ast::Pat::Ident(ident), Some(init)) = (&decl.name, &decl.init) {
                    let info = match &**init {
                        ast::Expr::Arrow(arrow) => Some(FunctionInfo {
                            span: arrow.span,
                            explicit_return: arrow.return_type.is_some(),
                        }),
                        ast::Expr::Fn(fn_expr) => Some(FunctionInfo {
                            span: fn_expr.function.span,
                            explicit_return: fn_expr.function.return_type.is_some(),
                        }),
                        _ => None,
                    };
                    if let Some(info) = info {
                        self.0
                            .functions
                            .entry(ident.id.sym.to_string())
                            .or_insert(info);
                    }
                }
                decl.visit_children_with(self);
            }
            fn visit_call_expr(&mut self, call: &ast::CallExpr) {
                if call.span.lo.0 != 0 {
                    self.0.calls.insert(call.span.lo.0, call.span);
                }
                call.visit_children_with(self);
            }
            fn visit_new_expr(&mut self, new_expr: &ast::NewExpr) {
                if new_expr.span.lo.0 != 0 {
                    self.0.calls.insert(new_expr.span.lo.0, new_expr.span);
                }
                new_expr.visit_children_with(self);
            }
            fn visit_ts_interface_decl(&mut self, decl: &ast::TsInterfaceDecl) {
                self.0.types.insert(decl.id.sym.to_string(), decl.span);
                decl.visit_children_with(self);
            }
            fn visit_ts_type_alias_decl(&mut self, decl: &ast::TsTypeAliasDecl) {
                self.0.types.insert(decl.id.sym.to_string(), decl.span);
                decl.visit_children_with(self);
            }
            fn visit_class_decl(&mut self, decl: &ast::ClassDecl) {
                self.0
                    .types
                    .insert(decl.ident.sym.to_string(), decl.class.span);
                decl.visit_children_with(self);
            }
            fn visit_import_decl(&mut self, import: &ast::ImportDecl) {
                self.0
                    .imports
                    .insert(import.src.value.to_string_lossy().into_owned(), import.span);
            }
        }
        module.visit_with(&mut Collector(self));
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
