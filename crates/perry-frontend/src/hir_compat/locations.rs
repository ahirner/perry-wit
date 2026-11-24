//! Original source coordinates for merged AST and HIR declarations.
use perry_hir::{
    ir::{Expr, Function, Module, Stmt},
    types::FuncId,
    walker::{stmt_any_expr, walk_expr_children},
};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Note, Result, SourceSpan, codes};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};
use swc_common::{Span, Spanned};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

const OPERATOR: &str = "\0perry_operator:";
const NON_NULL: &str = "\0perry_non_null";

pub(crate) fn has_source_offset(offset: u32) -> bool {
    offset != 0
}

pub(crate) fn mark_non_null(value: Expr) -> Expr {
    Expr::Sequence(vec![Expr::String(NON_NULL.into()), value])
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
        .strip_prefix(super::names::MODULE_PREFIX)
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
        note: Option<Note>,
    },
    Import(Note),
}

pub fn non_null_assertion(expr: &Expr) -> Option<&Expr> {
    let Expr::Sequence(parts) = expr else {
        return None;
    };
    match parts.as_slice() {
        [Expr::String(token), value] if token == NON_NULL => Some(value),
        _ => None,
    }
}

#[derive(Clone, Copy)]
struct FunctionInfo {
    span: Span,
    explicit_return: bool,
}

struct SourceFile<'s> {
    path: PathBuf,
    source: &'s str,
    positions: std::ops::RangeInclusive<u32>,
}

#[derive(Default)]
pub struct Locations<'s> {
    files: Vec<SourceFile<'s>>,
    functions: HashMap<String, FunctionInfo>,
    calls: HashMap<u32, (Span, Option<Span>)>,
    members: HashMap<u32, Span>,
    types: HashMap<String, Span>,
    imports: HashMap<String, Span>,
    operators: Vec<Span>,
    pub(crate) generic_templates: Vec<(FuncId, String)>,
    origins: HashMap<String, CallerOrigin>,
}

impl<'s> Locations<'s> {
    /// Preserve operator spans and non-null assertions as inert sequences before HIR erases them.
    pub(super) fn preserve_expressions(&mut self, module: &mut ast::Module, spans: &HashSet<Span>) {
        struct Preserver<'a>(&'a mut Vec<Span>, &'a HashSet<Span>);
        impl VisitMut for Preserver<'_> {
            // Enum initializers use the upstream constant evaluator, not MIR arithmetic.
            fn visit_mut_ts_enum_decl(&mut self, _: &mut ast::TsEnumDecl) {}

            fn visit_mut_expr(&mut self, expr: &mut ast::Expr) {
                expr.visit_mut_children_with(self);
                let span = expr.span();
                let token = if matches!(expr, ast::Expr::TsNonNull(_)) {
                    NON_NULL.to_owned()
                } else if self.1.contains(&span)
                    || matches!(expr, ast::Expr::Bin(binary) if matches!(
                        binary.op,
                        ast::BinaryOp::Exp | ast::BinaryOp::BitAnd | ast::BinaryOp::BitOr | ast::BinaryOp::BitXor
                        | ast::BinaryOp::LShift | ast::BinaryOp::RShift | ast::BinaryOp::ZeroFillRShift
                    ))
                {
                    let token = format!("{OPERATOR}{}", self.0.len());
                    self.0.push(span);
                    token
                } else {
                    return;
                };
                let token = ast::Expr::Lit(ast::Lit::Str(ast::Str {
                    span,
                    value: token.into(),
                    raw: None,
                }));
                let original = std::mem::replace(expr, ast::Expr::Invalid(ast::Invalid { span }));
                *expr = ast::Expr::Seq(ast::SeqExpr {
                    span,
                    exprs: vec![Box::new(token), Box::new(original)],
                });
            }
        }
        module.visit_mut_with(&mut Preserver(&mut self.operators, spans));
    }

    pub(crate) fn operator_raw_span(&self, parts: &[Expr]) -> Option<Span> {
        let [Expr::String(token), _] = parts else {
            return None;
        };
        let index: usize = token.strip_prefix(OPERATOR)?.parse().ok()?;
        self.operators.get(index).copied()
    }

    pub fn operator_span(&self, parts: &[Expr]) -> Option<SourceSpan> {
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

    pub fn display_function_name<'a>(&'a self, name: &'a str) -> &'a str {
        display_name(self.template_name(name))
    }

    pub(crate) fn record_instantiation(
        &mut self,
        instance: String,
        template: String,
        call_byte_offset: u32,
        caller: Option<String>,
    ) {
        let note = self.call_span(call_byte_offset).map(|span| {
            Note::spanned(
                span,
                format!("while instantiating '{}'", display_name(&template)),
            )
        });
        self.origins.insert(
            instance,
            CallerOrigin {
                site: CallSite::Instantiation { template, note },
                caller,
            },
        );
    }

    /// Keep the first cross-file caller, giving exported functions priority.
    pub(crate) fn record_call_origins(&mut self, module: &Module) {
        if self.files.len() <= 1 {
            return;
        }
        let names: HashMap<_, _> = module
            .functions
            .iter()
            .map(|function| (function.id, function.name.as_str()))
            .collect();
        let mut record = |id: FuncId, offset: u32, caller: Option<&str>| {
            let Some(&callee) = names.get(&id) else {
                return;
            };
            if self.origins.contains_key(callee) {
                return;
            }
            let Some(declaration) = self.functions.get(callee) else {
                return;
            };
            if let (Some(call), Some(target)) =
                (self.file_at(offset), self.file_at(declaration.span.lo.0))
                && call.path != target.path
                && let Some(span) = self.call_span(offset)
            {
                self.origins.insert(
                    callee.to_owned(),
                    CallerOrigin {
                        site: CallSite::Import(Note::spanned(
                            span,
                            format!("called from here ('{}')", display_name(callee)),
                        )),
                        caller: caller.map(str::to_owned),
                    },
                );
            }
        };
        body_calls(&module.init, &mut |id, offset| record(id, offset, None));
        for exported in [true, false] {
            for function in module
                .functions
                .iter()
                .filter(|f| f.is_exported == exported)
            {
                function_calls(function, &mut |id, offset| {
                    record(id, offset, Some(&function.name))
                });
            }
        }
        let mut class_call = |id, offset| record(id, offset, None);
        for class in &module.classes {
            for field in class.fields.iter().chain(&class.static_fields) {
                if let Some(init) = &field.init {
                    expression_calls(init, &mut class_call);
                }
            }
            for function in crate::shapes::class_functions(class) {
                function_calls(function, &mut class_call);
            }
        }
    }

    pub fn caller_notes(&self, function_name: &str) -> Vec<Note> {
        let mut notes = Vec::new();
        let mut seen = HashSet::new();
        let mut current = function_name;
        while let Some(origin) = self.origins.get(current)
            && seen.insert(current)
        {
            let note = match &origin.site {
                CallSite::Instantiation { note, .. } => note.as_ref(),
                CallSite::Import(note) => Some(note),
            };
            notes.extend(note.cloned());
            let Some(caller) = origin.caller.as_deref() else {
                break;
            };
            current = caller;
        }
        notes
    }

    pub fn annotate_caller(&self, error: Diagnostic, function_name: &str) -> Diagnostic {
        error.with_notes(self.caller_notes(function_name))
    }

    pub(crate) fn has_explicit_return(&self, name: &str) -> bool {
        self.functions
            .get(self.template_name(name))
            .is_some_and(|info| info.explicit_return)
    }

    pub fn function_span(&self, name: &str) -> Option<SourceSpan> {
        self.functions
            .get(self.template_name(name))
            .and_then(|info| self.span(info.span))
            .or_else(|| match self.origins.get(name)?.site {
                CallSite::Instantiation { ref note, .. } => {
                    note.as_ref().and_then(Note::span).cloned()
                }
                CallSite::Import(ref note) => note.span().cloned(),
            })
    }

    pub fn source_at(&self, position: u32) -> Option<&'s str> {
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

    pub fn offset_span(&self, start: u32, len: u32) -> Option<SourceSpan> {
        self.span(Span::new(
            swc_common::BytePos(start),
            swc_common::BytePos(start.saturating_add(len)),
        ))
    }

    pub(crate) fn calls_dispose(&self, start: u32, end: u32) -> bool {
        self.calls
            .get(&start)
            .and_then(|(_, dispose)| *dispose)
            .is_some_and(|span| span.hi.0 == end)
    }

    pub fn call_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.calls
            .get(&byte_offset)
            .and_then(|(span, _)| self.span(*span))
            .or_else(|| self.offset_span(byte_offset, 1))
    }

    pub fn member_span(&self, byte_offset: u32) -> Option<SourceSpan> {
        self.members
            .get(&byte_offset)
            .and_then(|span| self.span(*span))
            .or_else(|| self.call_span(byte_offset))
    }

    pub fn missing_function(&self, id: FuncId) -> Diagnostic {
        if let Some((_, name)) = self
            .generic_templates
            .iter()
            .find(|(template, _)| *template == id)
        {
            Diagnostic::unsupported(format!(
                "generic function '{}' requires a direct call for specialization",
                display_name(name)
            ))
        } else {
            Diagnostic::internal("call to an unknown function")
        }
    }

    pub fn type_span(&self, name: &str) -> Option<SourceSpan> {
        self.types.get(name).and_then(|span| self.span(*span))
    }

    pub fn import_span(&self, source: &str) -> Option<SourceSpan> {
        self.imports
            .get(source.strip_prefix("node:").unwrap_or(source))
            .and_then(|span| self.span(*span))
    }

    pub(crate) fn annotate_call(&self, error: Diagnostic, byte_offset: u32) -> Diagnostic {
        error.with_fallback_span(self.call_span(byte_offset))
    }

    pub(super) fn span(&self, span: Span) -> Option<SourceSpan> {
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

    pub(super) fn collect(
        &mut self,
        module: &perry_parser::swc_ecma_ast::Module,
        unresolved: swc_common::SyntaxContext,
    ) {
        struct Collector<'a, 's> {
            unresolved: swc_common::SyntaxContext,
            locations: &'a mut Locations<'s>,
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
                    let entry = self
                        .locations
                        .calls
                        .entry(call.span.lo.0)
                        .or_insert((call.span, None));
                    entry.0 = call.span;
                    if let ast::Callee::Expr(callee) = &call.callee
                        && let ast::Expr::Member(member) = callee.unwrap_parens()
                        && let ast::MemberProp::Computed(prop) = &member.prop
                        && let ast::Expr::Member(symbol) = prop.expr.unwrap_parens()
                        && matches!(&*symbol.obj, ast::Expr::Ident(id) if id.sym == "Symbol" && id.ctxt == self.unresolved)
                        && matches!(&symbol.prop,ast::MemberProp::Ident(id) if id.sym=="dispose")
                    {
                        entry.1 = Some(call.span);
                    }
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
                        .entry(new_expr.span.lo.0)
                        .or_insert((new_expr.span, None))
                        .0 = new_expr.span;
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
            unresolved,
            locations: self,
            class_name: None,
        });
    }
}

fn function_calls(function: &Function, visit: &mut impl FnMut(FuncId, u32)) {
    for default in function.params.iter().filter_map(|p| p.default.as_ref()) {
        expression_calls(default, visit);
    }
    body_calls(&function.body, visit);
}

fn body_calls(body: &[Stmt], visit: &mut impl FnMut(FuncId, u32)) {
    for statement in body {
        stmt_any_expr(statement, &mut |expr| {
            expression_calls(expr, visit);
            false
        });
    }
}

fn expression_calls(expr: &Expr, visit: &mut impl FnMut(FuncId, u32)) {
    if let Expr::Closure { body, .. } = expr {
        body_calls(body, visit);
    }
    walk_expr_children(expr, &mut |child| expression_calls(child, visit));
    if let Expr::Call {
        callee,
        byte_offset,
        ..
    } = expr
        && let Expr::FuncRef(id) = callee.as_ref()
    {
        visit(*id, *byte_offset);
    }
}
