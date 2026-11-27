//! Original source coordinates for merged AST and HIR declarations.
use perry_hir::ir::Expr;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, SourceSpan, codes};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use swc_common::{Span, Spanned};
use swc_ecma_visit::{Visit, VisitWith};

const OPERATOR: &str = "\0perry_operator:";
const NON_NULL: &str = "\0perry_non_null";

pub(crate) fn has_source_offset(offset: u32) -> bool {
    offset != 0
}

pub(crate) fn function_label(name: &str) -> &str {
    let name = display_name(name);
    if name.is_empty() || name.starts_with("callback_") {
        "anonymous function"
    } else {
        name
    }
}

/// Strip internal module-mangling prefixes (`__perry_module_<N>_`) and object-literal method
/// wrappers (`__obj_method_<name>_<N>`) for user-facing diagnostic names.
pub(crate) fn display_name(name: &str) -> &str {
    let name = name
        .strip_prefix(crate::source::MODULE_PREFIX)
        .and_then(|rest| {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            (digits > 0)
                .then(|| rest.get(digits..)?.strip_prefix('_'))
                .flatten()
        })
        .unwrap_or(name);
    name.strip_prefix("__obj_method_")
        .and_then(|rest| {
            let (base, suffix) = rest.rsplit_once('_')?;
            (!base.is_empty() && !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
                .then_some(base)
        })
        .unwrap_or(name)
}

struct CallerOrigin {
    site: CallSite,
    caller: Option<String>,
}

enum CallSite {
    Instantiation {
        template: String,
        offset: u32,
    },
    Import {
        offset: u32,
        priority: crate::source::CallerKind,
    },
}

pub(crate) fn non_null_assertion(expr: &Expr) -> Option<&Expr> {
    let Expr::Sequence(parts) = expr else {
        return None;
    };
    match parts.as_slice() {
        [Expr::String(token), value] if token == NON_NULL => Some(value),
        _ => None,
    }
}

struct FunctionInfo {
    label: Option<String>,
    span: Span,
    explicit_return: bool,
}

struct SourceFile<'s> {
    path: PathBuf,
    source: &'s str,
    positions: std::ops::RangeInclusive<u32>,
}

#[derive(Default)]
pub(crate) struct Locations<'s> {
    files: Vec<SourceFile<'s>>,
    functions: HashMap<String, FunctionInfo>,
    calls: HashMap<u32, Span>,
    members: HashMap<u32, Span>,
    types: HashMap<String, Span>,
    imports: HashMap<String, Span>,
    operators: Vec<Span>,
    origins: HashMap<String, CallerOrigin>,
}

impl<'s> Locations<'s> {
    pub(crate) fn preserve_expression(&mut self, expr: &mut ast::Expr) {
        use swc_common::util::take::Take;
        let span = expr.span();
        let token = if matches!(expr, ast::Expr::TsNonNull(_)) {
            NON_NULL.to_owned()
        } else {
            let token = format!("{OPERATOR}{}", self.operators.len());
            self.operators.push(span);
            token
        };
        let original = expr.take();
        *expr = ast::SeqExpr {
            span,
            exprs: vec![
                Box::new(
                    ast::Lit::Str(ast::Str {
                        span,
                        value: token.into(),
                        raw: None,
                    })
                    .into(),
                ),
                Box::new(original),
            ],
        }
        .into();
    }
    pub(crate) fn operator_raw_span(&self, parts: &[Expr]) -> Option<Span> {
        let [Expr::String(token), _] = parts else {
            return None;
        };
        let index: usize = token.strip_prefix(OPERATOR)?.parse().ok()?;
        self.operators.get(index).copied()
    }

    pub(crate) fn operator_span(&self, parts: &[Expr]) -> Option<SourceSpan> {
        self.operator_raw_span(parts)
            .and_then(|span| self.span(span))
    }

    pub(crate) fn add(&mut self, path: PathBuf, source: &'s str) -> Result<u32> {
        let offset = self.files.last().map_or(0, |file| *file.positions.end());
        let end = u32::try_from(u64::from(offset) + source.len() as u64 + 1).map_err(|_| {
            Diagnostic::new(
                codes::INVALID_INPUT,
                "TypeScript sources exceed the 32-bit span range",
            )
        })?;
        self.files.push(SourceFile {
            path,
            source,
            positions: offset + 1..=end,
        });
        Ok(offset)
    }

    pub(super) fn module_text(&self) -> String {
        let mut text = String::new();
        for file in &self.files {
            text.push_str(file.source);
            text.push('\n');
        }
        text
    }

    fn template_name<'a>(&'a self, mut name: &'a str) -> &'a str {
        let mut seen = HashSet::new();
        while let Some(CallSite::Instantiation { template, .. }) =
            self.origins.get(name).map(|origin| &origin.site)
            && seen.insert(name)
        {
            name = template;
        }
        name
    }

    pub(crate) fn display_function_name<'a>(&'a self, name: &'a str) -> &'a str {
        let name = self.template_name(name);
        self.functions
            .get(name)
            .and_then(|info| info.label.as_deref())
            .unwrap_or_else(|| display_name(name))
    }

    pub(super) fn register_generated(&mut self, symbol: String, span: Span, label: String) {
        self.functions.entry(symbol).or_insert(FunctionInfo {
            span,
            label: Some(label),
            explicit_return: false,
        });
    }

    pub(crate) fn record_instantiation(
        &mut self,
        instance: String,
        template: String,
        call_byte_offset: u32,
        caller: Option<String>,
    ) {
        self.origins.insert(
            instance,
            CallerOrigin {
                site: CallSite::Instantiation {
                    template,
                    offset: call_byte_offset,
                },
                caller,
            },
        );
    }

    pub(crate) fn has_imports(&self) -> bool {
        self.files.len() > 1
    }

    pub(crate) fn record_cross_file_call(
        &mut self,
        (callee, declaration): (&str, &str),
        offset: u32,
        caller: Option<&str>,
        priority: crate::source::CallerKind,
    ) {
        if self
            .origins
            .get(callee)
            .is_some_and(|origin| match &origin.site {
                CallSite::Instantiation { .. } => true,
                CallSite::Import {
                    priority: previous, ..
                } => *previous <= priority,
            })
        {
            return;
        }
        let Some(declaration) = self
            .functions
            .get(declaration)
            .filter(|info| info.label.is_none())
        else {
            return;
        };
        if let (Some(call), Some(target)) =
            (self.file_at(offset), self.file_at(declaration.span.lo.0))
            && call.path != target.path
        {
            self.origins.insert(
                callee.to_owned(),
                CallerOrigin {
                    site: CallSite::Import { priority, offset },
                    caller: caller.map(str::to_owned),
                },
            );
        }
    }

    fn caller_site<'a>(
        &'a self,
        name: &'a str,
        site: &'a CallSite,
    ) -> (perry_mir::CallerRelation, &'a str, Option<SourceSpan>) {
        let (kind, label, offset) = match site {
            CallSite::Instantiation { template, offset } => (
                perry_mir::CallerRelation::Instantiation,
                template.as_str(),
                *offset,
            ),
            CallSite::Import { offset, .. } => (perry_mir::CallerRelation::Import, name, *offset),
        };
        (
            kind,
            self.display_function_name(label),
            self.call_span(offset),
        )
    }

    pub(crate) fn set_mir_frames(
        &self,
        ctx: &mut pliron::context::Context,
        module: &pliron::builtin::ops::ModuleOp,
    ) {
        perry_mir::set_caller_frames(
            ctx,
            module,
            self.origins
                .iter()
                .map(|(name, origin)| {
                    let (kind, label, span) = self.caller_site(name, &origin.site);
                    perry_mir::CallerFrame::new(name, origin.caller.as_deref(), kind, label, span)
                })
                .collect(),
        );
    }

    pub(crate) fn set_mir_location(
        &self,
        ctx: &mut pliron::context::Context,
        function: &pliron::builtin::ops::FuncOp,
        name: &str,
    ) {
        if let Some(span) = self.function_span(name) {
            perry_mir::set_source_span(ctx, function, &span);
        }
        if self.origins.contains_key(name) {
            perry_mir::set_caller_origin(ctx, function, name);
        }
    }

    pub(crate) fn annotate_caller(&self, error: Diagnostic, function_name: &str) -> Diagnostic {
        let mut notes = Vec::new();
        let mut seen = HashSet::new();
        let mut current = function_name;
        while let Some(origin) = self.origins.get(current)
            && seen.insert(current)
        {
            let (kind, label, span) = self.caller_site(current, &origin.site);
            notes.extend(span.map(|span| kind.note(label, span)));
            let Some(caller) = origin.caller.as_deref() else {
                break;
            };
            current = caller;
        }
        error.with_notes(notes)
    }

    pub(crate) fn has_explicit_return(&self, name: &str) -> bool {
        self.functions
            .get(self.template_name(name))
            .is_some_and(|info| info.explicit_return)
    }

    pub(crate) fn declaration_span(&self, name: &str) -> Option<SourceSpan> {
        self.functions
            .get(self.template_name(name))
            .and_then(|info| self.span(info.span))
            .or_else(|| self.function_span(name))
    }

    pub(crate) fn function_span(&self, name: &str) -> Option<SourceSpan> {
        self.functions
            .get(self.template_name(name))
            .filter(|info| info.label.is_none())
            .and_then(|info| self.span(info.span))
            .or_else(|| match self.origins.get(name)?.site {
                CallSite::Instantiation { offset, .. } | CallSite::Import { offset, .. } => {
                    self.call_span(offset)
                }
            })
    }

    pub(crate) fn source_at(&self, position: u32) -> Option<&'s str> {
        let file = self.file_at(position)?;
        file.source
            .get((position - file.positions.start()) as usize..)
    }

    pub(super) fn source_text(&self, span: Span) -> Option<&'s str> {
        self.source_at(span.lo.0)?
            .get(..(span.hi.0.checked_sub(span.lo.0)?) as usize)
    }

    pub(crate) fn member_text(&self, offset: u32) -> Option<&'s str> {
        self.source_text(*self.members.get(&offset)?)
    }

    pub(crate) fn offset_span(&self, start: u32, len: u32) -> Option<SourceSpan> {
        self.span(Span::new(
            swc_common::BytePos(start),
            swc_common::BytePos(start.saturating_add(len)),
        ))
    }

    pub(crate) fn call_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.calls
            .get(&byte_offset)
            .and_then(|span| self.span(*span))
            .or_else(|| self.offset_span(byte_offset, 1))
    }

    pub(crate) fn member_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.members
            .get(&byte_offset)
            .and_then(|span| self.span(*span))
            .or_else(|| self.call_span(byte_offset))
    }

    pub(crate) fn type_span(&self, name: &str) -> Option<SourceSpan> {
        self.types.get(name).and_then(|span| self.span(*span))
    }

    pub(crate) fn import_span(&self, source: &str) -> Option<SourceSpan> {
        self.imports
            .get(source.strip_prefix("node:").unwrap_or(source))
            .and_then(|span| self.span(*span))
    }

    pub(crate) fn annotate_call(&self, error: Diagnostic, byte_offset: u32) -> Diagnostic {
        error.with_fallback_span(self.call_span(byte_offset))
    }

    pub(crate) fn span(&self, span: Span) -> Option<SourceSpan> {
        let file = self.file_at(span.lo.0)?;
        let start = (span.lo.0 - file.positions.start()) as usize;
        let end = span.hi.0.checked_sub(*file.positions.start())? as usize;
        (end <= file.source.len()).then(|| SourceSpan::new(&file.path, file.source, start, end))
    }

    fn file_at(&self, position: u32) -> Option<&SourceFile<'s>> {
        let index = self
            .files
            .partition_point(|file| *file.positions.end() < position);
        self.files
            .get(index)
            .filter(|file| file.positions.contains(&position))
    }

    pub(crate) fn annotate(&self, error: Diagnostic, span: Span) -> Diagnostic {
        error.with_fallback_span(self.span(span))
    }

    pub(super) fn collect(&mut self, module: &perry_parser::swc_ecma_ast::Module) {
        struct Collector<'a, 's> {
            locations: &'a mut Locations<'s>,
            class_name: Option<String>,
        }
        impl Visit for Collector<'_, '_> {
            fn visit_fn_decl(&mut self, function: &ast::FnDecl) {
                let name = function.ident.sym.to_string();
                self.locations.functions.insert(
                    name,
                    FunctionInfo {
                        label: None,
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
                            label: None,
                            span: arrow.span,
                            explicit_return: arrow.return_type.is_some(),
                        }),
                        ast::Expr::Fn(fn_expr) => Some(FunctionInfo {
                            label: None,
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
                    let token = match &call.callee {
                        ast::Callee::Expr(callee) => match callee.unwrap_with(|e| match e {
                            ast::Expr::Paren(p) => Some(&p.expr),
                            ast::Expr::TsAs(t) => Some(&t.expr),
                            ast::Expr::TsNonNull(t) => Some(&t.expr),
                            ast::Expr::TsSatisfies(t) => Some(&t.expr),
                            ast::Expr::TsTypeAssertion(t) => Some(&t.expr),
                            ast::Expr::TsConstAssertion(t) => Some(&t.expr),
                            _ => None,
                        }) {
                            ast::Expr::Member(m) => m.prop.span().lo,
                            ast::Expr::SuperProp(m) => m.prop.span().lo,
                            _ => call.span.lo,
                        },
                        _ => call.span.lo,
                    };
                    self.locations.calls.insert(token.0, call.span);
                }
                call.visit_children_with(self);
            }
            fn visit_member_expr(&mut self, member: &ast::MemberExpr) {
                self.locations
                    .members
                    .insert(member.prop.span().lo.0, member.span);
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
            fn visit_constructor(&mut self, ctor: &ast::Constructor) {
                if let Some(class_name) = &self.class_name {
                    self.locations.functions.insert(
                        format!("{class_name}::constructor"),
                        FunctionInfo {
                            label: None,
                            span: ctor.span,
                            explicit_return: false,
                        },
                    );
                }
                ctor.visit_children_with(self);
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
                                label: None,
                                span: method.function.span,
                                explicit_return: method.function.return_type.is_some(),
                            },
                        );
                    }
                }
                method.visit_children_with(self);
            }
            fn visit_import_decl(&mut self, import: &ast::ImportDecl) {
                let source = import.src.value.to_string_lossy();
                self.locations.imports.insert(
                    source.strip_prefix("node:").unwrap_or(&source).to_owned(),
                    import.span,
                );
            }
        }
        module.visit_with(&mut Collector {
            locations: self,
            class_name: None,
        });
    }
}
