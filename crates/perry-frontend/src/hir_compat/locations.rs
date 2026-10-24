//! Original source coordinates for merged AST and HIR declarations.
use perry_hir::ir::Expr;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{DiagnosticCode, PerryDiagnostic, SourceSpan};
use std::{collections::HashMap, path::PathBuf};
use swc_common::Span;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

const OPERATOR: &str = "\0perry_operator:";

#[derive(Clone, Copy)]
struct FunctionInfo {
    span: Span,
    explicit_return: bool,
}

#[derive(Default)]
pub struct Locations<'s> {
    files: Vec<(PathBuf, &'s str, u32)>,
    functions: HashMap<String, FunctionInfo>,
    calls: HashMap<u32, Span>,
    members: HashMap<u32, Span>,
    types: HashMap<String, Span>,
    imports: HashMap<String, Span>,
    operators: Vec<Span>,
    async_init: bool,
}

impl<'s> Locations<'s> {
    /// Preserve spans for unsupported binary operators as inert sequences before HIR erases binary spans.
    pub(super) fn preserve_operators(&mut self, module: &mut ast::Module) {
        struct Preserver<'a>(&'a mut Vec<Span>);
        impl VisitMut for Preserver<'_> {
            // Enum initializers use the upstream constant evaluator, not MIR arithmetic.
            fn visit_mut_ts_enum_decl(&mut self, _: &mut ast::TsEnumDecl) {}

            fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
                expr.visit_mut_children_with(self);
                let ast::Expr::Bin(binary) = expr else {
                    return;
                };
                if !matches!(
                    binary.op,
                    ast::BinaryOp::Exp
                        | ast::BinaryOp::BitAnd
                        | ast::BinaryOp::BitOr
                        | ast::BinaryOp::BitXor
                        | ast::BinaryOp::LShift
                        | ast::BinaryOp::RShift
                        | ast::BinaryOp::ZeroFillRShift
                ) {
                    return;
                }
                let span = binary.span;
                let token = ast::Expr::Lit(ast::Lit::Str(ast::Str {
                    span,
                    value: format!("{OPERATOR}{}", self.0.len()).into(),
                    raw: None,
                }));
                self.0.push(span);
                let original = std::mem::replace(expr, ast::Expr::Invalid(ast::Invalid { span }));
                *expr = ast::Expr::Seq(ast::SeqExpr {
                    span,
                    exprs: vec![Box::new(token), Box::new(original)],
                });
            }
        }
        module.visit_mut_with(&mut Preserver(&mut self.operators));
    }

    pub fn operator_span(&self, parts: &[Expr]) -> Option<SourceSpan> {
        let [Expr::String(token), _] = parts else {
            return None;
        };
        let index: usize = token.strip_prefix(OPERATOR)?.parse().ok()?;
        self.operators.get(index).and_then(|span| self.span(*span))
    }

    pub(crate) fn add(&mut self, file: PathBuf, source: &'s str, offset: u32) {
        self.files.push((file, source, offset));
    }

    pub fn async_init(&self) -> bool {
        self.async_init
    }

    pub(crate) fn has_explicit_return(&self, name: &str) -> bool {
        self.functions
            .get(name)
            .is_some_and(|info| info.explicit_return)
    }

    pub fn function_span(&self, name: &str) -> Option<SourceSpan> {
        self.functions
            .get(name)
            .and_then(|info| self.span(info.span))
    }

    pub fn offset_span(&self, start: u32, len: u32) -> Option<SourceSpan> {
        self.span(Span::new(
            swc_common::BytePos(start),
            swc_common::BytePos(start.saturating_add(len)),
        ))
    }

    pub fn call_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.calls
            .get(&byte_offset)
            .and_then(|span| self.span(*span))
            .or_else(|| self.offset_span(byte_offset, 1))
    }

    pub fn member_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.members
            .get(&byte_offset)
            .and_then(|span| self.span(*span))
            .or_else(|| self.call_span(byte_offset))
    }

    pub fn type_span(&self, name: &str) -> Option<SourceSpan> {
        self.types.get(name).and_then(|span| self.span(*span))
    }

    pub fn import_span(&self, source: &str) -> Option<SourceSpan> {
        self.imports.get(source).and_then(|span| self.span(*span))
    }

    pub(crate) fn annotate_call(&self, error: anyhow::Error, byte_offset: u32) -> anyhow::Error {
        Self::attach(error, self.call_span(byte_offset))
    }

    fn span(&self, span: Span) -> Option<SourceSpan> {
        self.files.iter().find_map(|(file, source, offset)| {
            let start = span.lo.0.checked_sub(offset + 1)? as usize;
            let end = span.hi.0.checked_sub(offset + 1)? as usize;
            (start <= source.len() && end <= source.len())
                .then(|| SourceSpan::new(file.clone(), source, start, end))
        })
    }

    pub(crate) fn annotate(&self, error: anyhow::Error, span: Span) -> anyhow::Error {
        Self::attach(error, self.span(span))
    }

    fn attach(error: anyhow::Error, span: Option<SourceSpan>) -> anyhow::Error {
        let mut diagnostic =
            PerryDiagnostic::from_error(error, DiagnosticCode::UnsupportedExpression);
        if diagnostic.span.is_none() {
            diagnostic.span = span;
        }
        diagnostic.into()
    }

    pub(super) fn collect(&mut self, module: &perry_parser::swc_ecma_ast::Module) {
        struct Collector<'a, 's> {
            locations: &'a mut Locations<'s>,
            in_fn: bool,
            class_name: Option<String>,
        }
        impl Visit for Collector<'_, '_> {
            fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
                let name = function.ident.sym.to_string();
                self.locations.functions.insert(
                    name,
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
                        let name = ident.id.sym.to_string();
                        self.locations.functions.entry(name).or_insert(info);
                    }
                }
                decl.visit_children_with(self);
            }
            fn visit_call_expr(&mut self, call: &ast::CallExpr) {
                if call.span.lo.0 != 0 {
                    self.locations.calls.insert(call.span.lo.0, call.span);
                }
                call.visit_children_with(self);
            }
            fn visit_member_expr(&mut self, member: &ast::MemberExpr) {
                self.locations.members.insert(member.span.lo.0, member.span);
                member.visit_children_with(self);
            }
            fn visit_new_expr(&mut self, new_expr: &ast::NewExpr) {
                if new_expr.span.lo.0 != 0 {
                    self.locations
                        .calls
                        .insert(new_expr.span.lo.0, new_expr.span);
                }
                new_expr.visit_children_with(self);
            }
            fn visit_ts_interface_decl(&mut self, decl: &ast::TsInterfaceDecl) {
                self.locations
                    .types
                    .insert(decl.id.sym.to_string(), decl.span);
                decl.visit_children_with(self);
            }
            fn visit_ts_type_alias_decl(&mut self, decl: &ast::TsTypeAliasDecl) {
                self.locations
                    .types
                    .insert(decl.id.sym.to_string(), decl.span);
                decl.visit_children_with(self);
            }
            fn visit_class_decl(&mut self, decl: &ast::ClassDecl) {
                self.locations
                    .types
                    .insert(decl.ident.sym.to_string(), decl.class.span);
                let previous = self.class_name.replace(decl.ident.sym.to_string());
                decl.visit_children_with(self);
                self.class_name = previous;
            }
            fn visit_class_method(&mut self, method: &ast::ClassMethod) {
                if let Some(class_name) = &self.class_name {
                    let name = match &method.key {
                        ast::PropName::Ident(name) => Some(name.sym.to_string()),
                        ast::PropName::Str(name) => name.value.as_str().map(str::to_owned),
                        _ => None,
                    };
                    if let Some(name) = name {
                        let prefix = match method.kind {
                            ast::MethodKind::Method => "",
                            ast::MethodKind::Getter => "get_",
                            ast::MethodKind::Setter => "set_",
                        };
                        self.locations.functions.insert(
                            format!("{class_name}::{prefix}{name}"),
                            FunctionInfo {
                                span: method.function.span,
                                explicit_return: method.function.return_type.is_some(),
                            },
                        );
                    }
                }
                method.visit_children_with(self);
            }
            fn visit_import_decl(&mut self, import: &ast::ImportDecl) {
                self.locations
                    .imports
                    .insert(import.src.value.to_string_lossy().into_owned(), import.span);
            }
            fn visit_await_expr(&mut self, expr: &ast::AwaitExpr) {
                if !self.in_fn {
                    self.locations.async_init = true;
                }
                expr.visit_children_with(self);
            }
            fn visit_function(&mut self, func: &ast::Function) {
                let prev = std::mem::replace(&mut self.in_fn, true);
                func.visit_children_with(self);
                self.in_fn = prev;
            }
            fn visit_arrow_expr(&mut self, arrow: &ast::ArrowExpr) {
                let prev = std::mem::replace(&mut self.in_fn, true);
                arrow.visit_children_with(self);
                self.in_fn = prev;
            }
        }
        module.visit_with(&mut Collector {
            locations: self,
            in_fn: false,
            class_name: None,
        });
    }
}
