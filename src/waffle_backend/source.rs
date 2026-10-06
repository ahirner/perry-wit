//! Resolve source bindings before Perry's name-based builtin lowering.

mod context;
mod date;
mod decoder;
mod filesystem;
mod http;
pub(crate) mod modules;
mod objects;
mod options;
mod readonly;
mod streams;
mod time;
pub(crate) use decoder::validate_lowering;

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail, ensure};
use perry_hir::types::Type as HirType;
use perry_parser::{parse_typescript, swc_ecma_ast as ast};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use super::capabilities::{
    CapabilityOperation, ClockOperation, ContextOperation, FilesystemOperation, LowerCapability,
    ProcessOperation, RandomOperation, StdioOperation,
};

#[derive(Default)]
pub(crate) struct SourceBindings {
    pub(crate) wit_imports: BTreeMap<String, String>,
    pub(crate) capabilities: BTreeMap<String, CapabilityOperation>,
    pub(crate) decoder_constructor: Option<String>,
    pub(crate) date_constructor: Option<String>,
    pub(crate) headers_constructor: Option<String>,
    pub(crate) request_constructor: Option<String>,
    pub(crate) response_constructor: Option<String>,
    pub(crate) abort_constructor: Option<String>,
    pub(crate) time_constructors: BTreeMap<String, super::time::TimeConstructor>,
}

fn underlying_expression(mut expression: &ast::Expr) -> &ast::Expr {
    loop {
        expression = match expression {
            ast::Expr::Paren(value) => &value.expr,
            ast::Expr::TsAs(value) => &value.expr,
            ast::Expr::TsSatisfies(value) => &value.expr,
            ast::Expr::TsNonNull(value) => &value.expr,
            ast::Expr::TsTypeAssertion(value) => &value.expr,
            _ => return expression,
        };
    }
}

pub(crate) fn resolve_bindings(
    module: &mut ast::Module,
    wit: Option<&super::wit::WitWorld>,
) -> Result<SourceBindings> {
    GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        readonly::validate(module, SyntaxContext::empty().apply_mark(unresolved))?;
        let wit_imports = wit
            .map(|wit| wit.bind_source(module, unresolved))
            .transpose()?
            .unwrap_or_default();
        let mut names = IdentifierNames::default();
        module.visit_with(&mut names);
        ensure!(
            !names.0.contains(super::abort::Kind::Controller.type_name())
                && !names.0.contains(super::abort::Kind::Signal.type_name())
                && !names.0.contains(super::values::VALUE_TYPE)
                && !names.0.contains(super::context::ENVIRONMENT_TYPE)
                && !names.0.contains(super::date::DATE_TYPE)
                && !names.0.contains(super::http::RESPONSE_TYPE)
                && !names.0.contains(super::http::fetch::RESPONSE_TYPE)
                && !names.0.contains(super::http::headers::HEADERS_TYPE)
                && !names.0.contains(super::http::request::REQUEST_TYPE)
                && !names.0.contains(super::objects::INFERRED_RECORD_TYPE)
                && !names.0.contains(super::streams::web::Kind::Readable.name())
                && !names.0.contains(super::streams::web::Kind::Reader.name())
                && !names.0.contains(super::streams::web::Kind::Writable.name())
                && !names.0.contains(super::streams::web::Kind::Writer.name())
                && !names.0.contains(super::time::TimeKind::Instant.type_name())
                && !names
                    .0
                    .contains(super::time::TimeKind::PlainDateTime.type_name())
                && !names.0.contains(super::decoder::DECODER_TYPE)
                && !names.0.iter().any(|name| name.starts_with("__AnonShape_")),
            "Reserved compiler type name in source"
        );
        let mut bindings = HashMap::new();
        let mut http_types = HashSet::new();
        for item in &module.body {
            let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
                continue;
            };
            if import.src.value.as_str() == Some("perry:http") {
                for specifier in &import.specifiers {
                    if let ast::ImportSpecifier::Named(named) = specifier {
                        let name = named.imported.as_ref().map_or_else(
                            || named.local.sym.to_string(),
                            |name| name.atom().to_string(),
                        );
                        if name == "HttpResponse" {
                            ensure!(
                                import.type_only || named.is_type_only,
                                "Import HttpResponse with import type"
                            );
                            http_types.insert(named.local.to_id());
                        }
                    }
                }
            }
            if import.type_only {
                continue;
            }
            let namespace = match import.src.value.as_str() {
                Some("node:timers/promises") => CapabilityNamespace::TimerPromises,
                Some("node:stream") => CapabilityNamespace::Stream,
                Some("perry:http") => CapabilityNamespace::Http,
                Some("fs" | "node:fs") => CapabilityNamespace::Filesystem,
                Some("fs/promises" | "node:fs/promises") => CapabilityNamespace::FilesystemPromises,
                _ => bail!("Unsupported capability import: {:?}", import.src.value),
            };
            ensure!(
                !import.specifiers.is_empty(),
                "Capability imports cannot run module initialization"
            );
            for specifier in &import.specifiers {
                let binding = match specifier {
                    ast::ImportSpecifier::Named(named) if named.is_type_only => continue,
                    ast::ImportSpecifier::Named(named) => {
                        let name = named.imported.as_ref().map_or_else(
                            || named.local.sym.to_string(),
                            |name| name.atom().to_string(),
                        );
                        CapabilityBinding::Operation(namespace.operation(&name)?)
                    }
                    ast::ImportSpecifier::Namespace(_) => CapabilityBinding::Namespace(namespace),
                    ast::ImportSpecifier::Default(_)
                        if matches!(
                            namespace,
                            CapabilityNamespace::Filesystem
                                | CapabilityNamespace::FilesystemPromises
                        ) =>
                    {
                        CapabilityBinding::Namespace(namespace)
                    }
                    ast::ImportSpecifier::Default(_) => {
                        bail!("Capability modules have no default export")
                    }
                };
                bindings.insert(specifier.local().to_id(), binding);
            }
        }
        module.body.retain(|item| {
            !matches!(
                item,
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(_))
            )
        });
        let mut calls = SourceCalls {
            bindings,
            http_types,
            unresolved: SyntaxContext::empty().apply_mark(unresolved),
            names: names.0,
            shadow_names: HashMap::new(),
            operations: BTreeMap::new(),
            decoder_constructor: None,
            date_constructor: None,
            headers_constructor: None,
            request_constructor: None,
            response_constructor: None,
            abort_constructor: None,
            time_constructors: BTreeMap::new(),
            error: None,
        };
        module.visit_mut_with(&mut calls);
        if let Some(error) = calls.error {
            return Err(error);
        }
        let mut resolved = SourceBindings {
            wit_imports,
            decoder_constructor: calls.decoder_constructor,
            date_constructor: calls.date_constructor,
            headers_constructor: calls.headers_constructor,
            request_constructor: calls.request_constructor,
            response_constructor: calls.response_constructor,
            abort_constructor: calls.abort_constructor,
            time_constructors: calls
                .time_constructors
                .into_iter()
                .map(|(operation, name)| (name, operation))
                .collect(),
            ..Default::default()
        };
        for (operation, name) in calls.operations {
            let plan = operation.lower();
            let parameters = plan
                .params
                .iter()
                .enumerate()
                .map(|(index, ty)| Ok(format!("arg{index}: {}", source_type(ty)?)))
                .collect::<Result<Vec<_>>>()?
                .join(", ");
            let declaration = format!(
                "declare function {name}({parameters}): {};",
                source_type(&plan.result)?
            );
            let mut declaration = parse_typescript(&declaration, "capability.d.ts")?;
            module.body.append(&mut declaration.body);
            resolved.capabilities.insert(name, operation);
        }
        if let Some(name) = &resolved.abort_constructor {
            let declaration = format!(
                "declare function {name}(): {};",
                super::abort::Kind::Controller.type_name()
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "abort.d.ts")?.body);
        }
        if let Some(name) = &resolved.decoder_constructor {
            let declaration = format!(
                "declare function {name}(label: any, options: any): {};",
                super::decoder::DECODER_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "decoder.d.ts")?.body);
        }
        if let Some(name) = &resolved.date_constructor {
            let declaration = format!(
                "declare function {name}(value: number): {};",
                super::date::DATE_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "date.d.ts")?.body);
        }
        if let Some(name) = &resolved.headers_constructor {
            let declaration = format!(
                "declare function {name}(init: any): {};",
                super::http::headers::HEADERS_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "headers.d.ts")?.body);
        }
        if let Some(name) = &resolved.request_constructor {
            let declaration = format!(
                "declare function {name}(input: any, init: any): {};",
                super::http::request::REQUEST_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "request.d.ts")?.body);
        }
        if let Some(name) = &resolved.response_constructor {
            let declaration = format!(
                "declare function {name}(body: any, init: any): {};",
                super::http::fetch::RESPONSE_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "response.d.ts")?.body);
        }
        for (name, operation) in &resolved.time_constructors {
            let declaration = format!(
                "declare function {name}(value: {}): {};",
                source_type(&operation.argument_type())?,
                operation.kind().type_name()
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "temporal.d.ts")?.body);
        }
        Ok(resolved)
    })
}

fn source_type(ty: &HirType) -> Result<String> {
    match ty {
        HirType::Named(name) if super::http::is_response(ty) => Ok(name.clone()),
        ty if *ty == super::http::headers_type() => Ok("{[key: string]: string}".into()),
        ty if super::text_or_bytes::is_text_or_bytes(ty) => Ok("string | Uint8Array".into()),
        ty if *ty == ProcessOperation::GetExitCode.lower().result => {
            Ok("number | undefined".into())
        }
        ty if super::streams::web::Kind::of(ty).is_some() => {
            Ok(super::streams::web::Kind::of(ty).unwrap().name().into())
        }
        HirType::Number => Ok("number".into()),
        HirType::Boolean => Ok("boolean".into()),
        HirType::String => Ok("string".into()),
        HirType::Void => Ok("void".into()),
        HirType::Any => Ok("any".into()),
        ty if super::context::is_environment(ty) => Ok(super::context::ENVIRONMENT_TYPE.into()),
        HirType::Promise(inner) => Ok(format!("Promise<{}>", source_type(inner)?)),
        ty if super::bytes::is_byte_view(ty) => Ok("Uint8Array".into()),
        ty if super::filesystem::is_stats(ty) => Ok("Stats".into()),
        HirType::Array(inner) if **inner == HirType::String => Ok("string[]".into()),
        _ => bail!("Unsupported capability source type: {ty:?}"),
    }
}

#[derive(Clone, Copy)]
enum CapabilityNamespace {
    TimerPromises,
    Stream,
    Filesystem,
    FilesystemPromises,
    Http,
}

impl CapabilityNamespace {
    fn operation(self, name: &str) -> Result<CapabilityOperation> {
        if matches!(self, Self::FilesystemPromises) {
            let operation = match name {
                "readFile" => FilesystemOperation::ReadBytes,
                "writeFile" => FilesystemOperation::WriteFile,
                "stat" => FilesystemOperation::Stat,
                "mkdir" => FilesystemOperation::MakeDirectory,
                "unlink" => FilesystemOperation::Unlink,
                "rmdir" => FilesystemOperation::RemoveDirectory,
                "readdir" => FilesystemOperation::ReadDirectory,
                _ => bail!("Unsupported promise-based filesystem operation '{name}'"),
            };
            return Ok(CapabilityOperation::Filesystem(operation));
        }
        match (self, name) {
            (Self::Http, "get") => Ok(CapabilityOperation::HttpGet),
            (Self::TimerPromises, "setTimeout") => {
                Ok(CapabilityOperation::Clock(ClockOperation::Timeout))
            }
            (Self::Stream, "Writable") => {
                Ok(CapabilityOperation::Writable(StdioOperation::WriteStdout))
            }
            (Self::Filesystem, _) => bail!(
                "Unsupported filesystem operation '{name}': synchronous and callback APIs are not supported; use node:fs/promises"
            ),
            _ => bail!("Unknown capability member '{name}'"),
        }
    }
}

enum CapabilityBinding {
    Operation(CapabilityOperation),
    Namespace(CapabilityNamespace),
}

struct SourceCalls {
    http_types: HashSet<ast::Id>,
    bindings: HashMap<ast::Id, CapabilityBinding>,
    unresolved: SyntaxContext,
    names: HashSet<String>,
    shadow_names: HashMap<ast::Id, String>,
    operations: BTreeMap<CapabilityOperation, String>,
    decoder_constructor: Option<String>,
    date_constructor: Option<String>,
    headers_constructor: Option<String>,
    request_constructor: Option<String>,
    response_constructor: Option<String>,
    abort_constructor: Option<String>,
    time_constructors: BTreeMap<super::time::TimeConstructor, String>,
    error: Option<anyhow::Error>,
}

impl SourceCalls {
    fn validate_json_call(&self, call: &ast::CallExpr, callee: &ast::Expr) -> Result<()> {
        let ast::Expr::Member(member) = underlying_expression(callee) else {
            return Ok(());
        };
        if !matches!(underlying_expression(&member.obj), ast::Expr::Ident(name) if name.sym == "JSON" && name.ctxt == self.unresolved)
        {
            return Ok(());
        }
        let name = match &member.prop {
            ast::MemberProp::Ident(name) => name.sym.as_ref(),
            ast::MemberProp::Computed(key) => match key.expr.as_ref() {
                ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str().unwrap_or(""),
                _ => bail!("Dynamic JSON member lookup is unsupported"),
            },
            _ => bail!("Private JSON member lookup is unsupported"),
        };
        let maximum = match name {
            "parse" => 2,
            "stringify" => 3,
            _ => bail!("Unsupported JSON method '{name}'"),
        };
        ensure!(
            (1..=maximum).contains(&call.args.len()),
            "JSON.{name} has an unsupported argument count"
        );
        ensure!(
            call.args.iter().all(|arg| arg.spread.is_none()),
            "Spread JSON arguments are unsupported"
        );
        for argument in call.args.iter().skip(1) {
            ensure!(
                matches!(argument.expr.as_ref(), ast::Expr::Lit(ast::Lit::Null(_)))
                    || matches!(argument.expr.as_ref(), ast::Expr::Ident(name) if name.sym == "undefined" && name.ctxt == self.unresolved),
                "JSON revivers, replacers, and indentation are unsupported"
            );
        }
        Ok(())
    }

    fn reject_regexp_constructor(&mut self, callee: &ast::Expr) -> bool {
        if matches!(callee, ast::Expr::Ident(name) if name.sym == "RegExp" && name.ctxt == self.unresolved)
        {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!(
                    "RegExp construction is unsupported; use a regex literal in string.search"
                )
            });
            true
        } else {
            false
        }
    }

    fn operation(&self, expression: &ast::Expr) -> Result<Option<CapabilityOperation>> {
        match expression {
            ast::Expr::Ident(ident) if ident.sym == "fetch" && ident.ctxt == self.unresolved => {
                Ok(Some(CapabilityOperation::Fetch))
            }
            ast::Expr::Ident(ident) => match self.bindings.get(&ident.to_id()) {
                Some(CapabilityBinding::Operation(operation)) => Ok(Some(*operation)),
                Some(CapabilityBinding::Namespace(_)) => {
                    bail!("Capability namespaces cannot be used as values")
                }
                None => Ok(None),
            },
            ast::Expr::Member(member) => {
                let ast::Expr::Ident(receiver) = member.obj.as_ref() else {
                    return Ok(None);
                };
                let namespace = match self.bindings.get(&receiver.to_id()) {
                    Some(CapabilityBinding::Namespace(namespace)) => Some(*namespace),
                    _ => None,
                };
                let builtin_math = receiver.sym == "Math" && receiver.ctxt == self.unresolved;
                let builtin_console = receiver.sym == "console" && receiver.ctxt == self.unresolved;
                let builtin_crypto = receiver.sym == "crypto" && receiver.ctxt == self.unresolved;
                let builtin_performance =
                    receiver.sym == "performance" && receiver.ctxt == self.unresolved;
                let builtin_date = receiver.sym == "Date" && receiver.ctxt == self.unresolved;
                let builtin_process = receiver.sym == "process" && receiver.ctxt == self.unresolved;
                let builtin_promise = receiver.sym == "Promise" && receiver.ctxt == self.unresolved;
                if namespace.is_none()
                    && !builtin_math
                    && !builtin_console
                    && !builtin_crypto
                    && !builtin_performance
                    && !builtin_date
                    && !builtin_process
                    && !builtin_promise
                {
                    return Ok(None);
                }
                let name = match &member.prop {
                    ast::MemberProp::Ident(ident) => ident.sym.as_ref(),
                    ast::MemberProp::Computed(computed) => match computed.expr.as_ref() {
                        ast::Expr::Lit(ast::Lit::Str(text)) => text.value.as_str().unwrap_or(""),
                        _ => bail!("Dynamic capability member lookup is unsupported"),
                    },
                    _ => bail!("Private capability member lookup is unsupported"),
                };
                if builtin_promise {
                    let operation = super::promises::Combinator::from_name(name)
                        .with_context(|| format!("Promise.{name} is unsupported; factories and dynamic callbacks are deferred under D5"))?;
                    Ok(Some(CapabilityOperation::Promise(operation)))
                } else if let Some(namespace) = namespace {
                    Ok(Some(namespace.operation(name)?))
                } else if builtin_console {
                    Ok(Some(CapabilityOperation::Stdio(match name {
                        "log" => StdioOperation::Log,
                        "error" | "warn" => StdioOperation::Error,
                        _ => bail!("Unsupported console method '{name}'"),
                    })))
                } else if builtin_crypto {
                    Ok(Some(CapabilityOperation::Random(match name {
                        "getRandomValues" => RandomOperation::Fill,
                        "randomUUID" => RandomOperation::Uuid,
                        _ => bail!("Unsupported crypto method '{name}'"),
                    })))
                } else if builtin_performance {
                    ensure!(name == "now", "Unsupported performance method '{name}'");
                    Ok(Some(CapabilityOperation::Clock(
                        ClockOperation::MonotonicNow,
                    )))
                } else if builtin_date {
                    ensure!(name == "now", "Unsupported Date static method '{name}'");
                    Ok(Some(CapabilityOperation::Clock(ClockOperation::DateNow)))
                } else if builtin_process {
                    Ok(Some(match name {
                        "cwd" => CapabilityOperation::Context(ContextOperation::InitialCwd),
                        "exit" => CapabilityOperation::Process(ProcessOperation::Exit),
                        _ => bail!("Unsupported process operation '{name}'"),
                    }))
                } else if name == "random" {
                    Ok(Some(CapabilityOperation::Random(RandomOperation::Number)))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    fn capability_name(&mut self, operation: CapabilityOperation) -> String {
        if let Some(name) = self.operations.get(&operation) {
            return name.clone();
        }
        let name = self.fresh_name();
        self.operations.insert(operation, name.clone());
        name
    }

    fn fresh_name(&mut self) -> String {
        let mut index = self.names.len();
        loop {
            let name = format!("__perry_resolved_{index}");
            if self.names.insert(name.clone()) {
                return name;
            }
            index += 1;
        }
    }
}

impl VisitMut for SourceCalls {
    fn visit_mut_stmt(&mut self, statement: &mut ast::Stmt) {
        statement.visit_mut_children_with(self);
        if let ast::Stmt::ForOf(loop_) = statement
            && loop_.is_await
        {
            match self.rewrite_stream_iteration(loop_) {
                Ok(lowered) => *statement = lowered,
                Err(error) => {
                    self.error.get_or_insert(error);
                }
            }
        }
    }

    /// SWC already decoded the literal; Perry's raw-text encoding repair corrupts valid Latin-1 text.
    fn visit_mut_str(&mut self, literal: &mut ast::Str) {
        literal.raw = None;
    }

    fn visit_mut_ts_property_signature(&mut self, property: &mut ast::TsPropertySignature) {
        if property.computed {
            property.key.visit_mut_with(self);
        }
        property.type_ann.visit_mut_with(self);
    }

    fn visit_mut_prop(&mut self, property: &mut ast::Prop) {
        if let ast::Prop::Shorthand(id) = property {
            let key = ast::PropName::Ident(ast::IdentName::new(id.sym.clone(), id.span));
            let mut value = id.clone();
            value.visit_mut_with(self);
            *property = ast::Prop::KeyValue(ast::KeyValueProp {
                key,
                value: Box::new(ast::Expr::Ident(value)),
            });
        } else {
            property.visit_mut_children_with(self);
        }
    }

    fn visit_mut_var_declarator(&mut self, declaration: &mut ast::VarDeclarator) {
        // Perry approximates factory calls in inferred record fields as any.
        // Preserve explicit annotations; let SSA infer unannotated literal fields.
        if let ast::Pat::Ident(binding) = &mut declaration.name
            && binding.type_ann.is_none()
            && declaration
                .init
                .as_deref()
                .is_some_and(|value| matches!(underlying_expression(value), ast::Expr::Object(_)))
        {
            binding.type_ann = Some(Box::new(ast::TsTypeAnn {
                span: declaration.span,
                type_ann: Box::new(ast::TsType::TsTypeRef(ast::TsTypeRef {
                    span: declaration.span,
                    type_name: ast::TsEntityName::Ident(ast::Ident::new(
                        super::objects::INFERRED_RECORD_TYPE.into(),
                        declaration.span,
                        SyntaxContext::empty(),
                    )),
                    type_params: None,
                })),
            }));
        }
        declaration.visit_mut_children_with(self);
    }

    fn visit_mut_array_lit(&mut self, array: &mut ast::ArrayLit) {
        if array.elems.iter().any(Option::is_none) {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!("Array elisions are unsupported; arrays must be dense")
            });
            return;
        }
        array.visit_mut_children_with(self);
    }

    fn visit_mut_unary_expr(&mut self, expression: &mut ast::UnaryExpr) {
        if expression.op == ast::UnaryOp::Delete {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!("Runtime delete is unsupported by the static TypeScript contract")
            });
            return;
        }
        expression.visit_mut_children_with(self);
    }

    fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
        call.ctxt = SyntaxContext::empty();
        match self.rewrite_time_call(call) {
            Ok(true) => {
                call.args.visit_mut_with(self);
                return;
            }
            Err(error) => {
                self.error.get_or_insert(error);
                return;
            }
            Ok(false) => {}
        }
        if let Err(error) = self.validate_object_call(call) {
            self.error.get_or_insert(error);
            return;
        }
        if let Err(error) = decoder::validate_decode_options(call) {
            self.error.get_or_insert(error);
            return;
        }
        if let ast::Callee::Expr(callee) = &call.callee
            && let Err(error) = self.validate_json_call(call, callee)
        {
            self.error.get_or_insert(error);
            return;
        }
        if let ast::Callee::Expr(callee) = &mut call.callee {
            // Perry may fold constructors before argument effects are retained.
            if self.reject_regexp_constructor(callee) {
                return;
            }
            match self.operation(callee) {
                Ok(Some(mut operation)) => {
                    if matches!(operation, CapabilityOperation::Writable(_)) {
                        self.error.get_or_insert_with(|| {
                            anyhow::anyhow!("Writable supports only toWeb(process.stdout/stderr)")
                        });
                        return;
                    }
                    if operation == CapabilityOperation::Process(ProcessOperation::Exit)
                        && call.args.is_empty()
                    {
                        operation = CapabilityOperation::Process(ProcessOperation::ExitCurrent);
                    }
                    if matches!(
                        operation,
                        CapabilityOperation::Clock(ClockOperation::Timeout)
                            | CapabilityOperation::Process(ProcessOperation::Exit)
                    ) {
                        let default = ast::ExprOrSpread {
                            spread: None,
                            expr: Box::new(ast::Expr::Lit(ast::Lit::Num(ast::Number {
                                span: call.span,
                                value: if operation
                                    == CapabilityOperation::Process(ProcessOperation::Exit)
                                {
                                    0.0
                                } else {
                                    1.0
                                },
                                raw: None,
                            }))),
                        };
                        if call.args.is_empty() {
                            call.args.push(default);
                        } else if matches!(underlying_expression(&call.args[0].expr), ast::Expr::Ident(name) if name.sym == "undefined" && name.ctxt == self.unresolved)
                        {
                            call.args[0] = default;
                        }
                    }
                    if operation == CapabilityOperation::Clock(ClockOperation::Timeout) {
                        if call.args.len() > 3 {
                            self.error.get_or_insert_with(|| anyhow::anyhow!(
                                "setTimeout accepts a delay, result value, and optional timer options"
                            ));
                            return;
                        }
                        if call.args.len() >= 2 {
                            operation = CapabilityOperation::Clock(ClockOperation::TimeoutValue);
                        }
                    }
                    let operation =
                        match filesystem::specialize(operation, &call.args, self.unresolved) {
                            Ok(operation) => operation,
                            Err(error) => {
                                self.error.get_or_insert(error);
                                return;
                            }
                        };
                    if call.args.iter().any(|argument| argument.spread.is_some()) {
                        self.error.get_or_insert_with(|| {
                            anyhow::anyhow!("Spread capability arguments are unsupported")
                        });
                        return;
                    }
                    let name = self.capability_name(operation);
                    **callee = ast::Expr::Ident(ast::Ident::new(
                        name.into(),
                        call.span,
                        SyntaxContext::empty(),
                    ));
                    call.args.visit_mut_with(self);
                    return;
                }
                Err(error) => {
                    self.error.get_or_insert(error);
                    return;
                }
                Ok(None) => {}
            }
        }
        call.visit_mut_children_with(self);
    }

    fn visit_mut_new_expr(&mut self, expression: &mut ast::NewExpr) {
        if matches!(expression.callee.as_ref(), ast::Expr::Ident(name) if name.sym == "Uint8Array" && name.ctxt == self.unresolved)
        {
            let arguments = expression.args.as_deref().unwrap_or_default();
            if arguments.len() > 1 || arguments.iter().any(|argument| argument.spread.is_some()) {
                self.error.get_or_insert_with(|| anyhow::anyhow!("Uint8Array construction supports one non-spread argument; explicit byteOffset/length overloads are unsupported"));
                return;
            }
        }
        if !self.reject_regexp_constructor(&expression.callee) {
            expression.visit_mut_children_with(self);
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if let Err(error) = self.rewrite_writable(expression) {
            self.error.get_or_insert(error);
            return;
        }
        self.rewrite_process_assignment(expression);
        self.rewrite_process_value(expression);
        if matches!(expression, ast::Expr::Object(_))
            && let Err(error) = options::validate_plain_options(expression, "Object")
        {
            self.error.get_or_insert(error);
            return;
        }
        if let Err(error) = self.rewrite_http_constructor(expression) {
            self.error.get_or_insert(error);
            return;
        }
        if let Err(error) = self.rewrite_date_constructor(expression) {
            self.error.get_or_insert(error);
            return;
        }
        if let Err(error) = self.rewrite_decoder_constructor(expression) {
            self.error.get_or_insert(error);
            return;
        }
        if !matches!(expression, ast::Expr::Call(_)) {
            match self.operation(expression) {
                Ok(Some(_)) => {
                    self.error.get_or_insert_with(|| {
                        anyhow::anyhow!("Capability functions are only supported as direct calls")
                    });
                }
                Err(error) => {
                    self.error.get_or_insert(error);
                }
                Ok(None) => {}
            }
        }
        expression.visit_mut_children_with(self);
    }

    fn visit_mut_ident(&mut self, ident: &mut ast::Ident) {
        if ident.sym == "Temporal" && ident.ctxt == self.unresolved {
            self.error.get_or_insert_with(|| anyhow::anyhow!("Temporal supports only direct calls to Instant.from, Instant.fromEpochMilliseconds, and PlainDateTime.from"));
        }
        if matches!(
            ident.sym.as_ref(),
            "Math"
                | "RegExp"
                | "JSON"
                | "fetch"
                | "Uint8Array"
                | "TextDecoder"
                | "console"
                | "crypto"
                | "performance"
                | "Date"
                | "Temporal"
                | "Promise"
                | "process"
                | "Object"
                | "Array"
        ) && ident.ctxt != self.unresolved
        {
            let id = ident.to_id();
            let name = if let Some(name) = self.shadow_names.get(&id) {
                name.clone()
            } else {
                let name = self.fresh_name();
                self.shadow_names.insert(id, name.clone());
                name
            };
            ident.sym = name.into();
        }
        ident.ctxt = SyntaxContext::empty();
    }

    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }

    fn visit_mut_ts_type_ref(&mut self, reference: &mut ast::TsTypeRef) {
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.ctxt == self.unresolved
            && matches!(
                name.sym.as_ref(),
                "ReadableStream"
                    | "ReadableStreamDefaultReader"
                    | "WritableStream"
                    | "WritableStreamDefaultWriter"
            )
        {
            let bytes = reference.type_params.as_ref().is_some_and(|params| {
                params.params.len() == 1
                    && matches!(params.params[0].as_ref(),
                    ast::TsType::TsTypeRef(ty) if matches!(&ty.type_name,
                        ast::TsEntityName::Ident(name) if name.sym == "Uint8Array"))
            });
            if !bytes {
                self.error.get_or_insert_with(|| {
                    anyhow::anyhow!("Native Web Streams require Uint8Array chunks")
                });
                return;
            }
            let kind = match name.sym.as_ref() {
                "ReadableStream" => super::streams::web::Kind::Readable,
                "ReadableStreamDefaultReader" => super::streams::web::Kind::Reader,
                "WritableStream" => super::streams::web::Kind::Writable,
                _ => super::streams::web::Kind::Writer,
            };
            name.sym = kind.name().into();
            reference.type_params = None;
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.ctxt == self.unresolved
        {
            match name.sym.as_ref() {
                "AbortController" => name.sym = super::abort::Kind::Controller.type_name().into(),
                "AbortSignal" => name.sym = super::abort::Kind::Signal.type_name().into(),
                _ => {}
            }
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.sym == "Request"
            && name.ctxt == self.unresolved
        {
            name.sym = super::http::request::REQUEST_TYPE.into();
        }

        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.sym == "Headers"
            && name.ctxt == self.unresolved
        {
            name.sym = super::http::headers::HEADERS_TYPE.into();
        }

        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.sym == "Response"
            && name.ctxt == self.unresolved
        {
            name.sym = super::http::fetch::RESPONSE_TYPE.into();
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && self.http_types.contains(&name.to_id())
        {
            name.sym = super::http::RESPONSE_TYPE.into();
        }
        if let Err(error) = self.rewrite_time_type(reference) {
            self.error.get_or_insert(error);
            return;
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.sym == "TextDecoder"
            && name.ctxt == self.unresolved
        {
            name.sym = super::decoder::DECODER_TYPE.into();
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.sym == "Date"
            && name.ctxt == self.unresolved
        {
            name.sym = super::date::DATE_TYPE.into();
        }
        reference.visit_mut_children_with(self);
    }
}

#[derive(Default)]
struct IdentifierNames(HashSet<String>);

impl Visit for IdentifierNames {
    fn visit_ident(&mut self, ident: &ast::Ident) {
        self.0.insert(ident.sym.to_string());
    }
}
