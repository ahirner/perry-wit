//! Resolve source bindings before Perry's name-based builtin lowering.

mod decoder;
mod filesystem;
mod options;
pub(crate) use decoder::validate_lowering;

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Result, bail, ensure};
use perry_hir::types::Type as HirType;
use perry_parser::{parse_typescript, swc_ecma_ast as ast};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

use super::capabilities::{
    CapabilityOperation, ClockOperation, FilesystemOperation, LowerCapability, RandomOperation,
    StdioOperation,
};

#[derive(Default)]
pub(crate) struct SourceBindings {
    pub(crate) capabilities: BTreeMap<String, CapabilityOperation>,
    pub(crate) decoder_constructor: Option<String>,
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

pub(crate) fn resolve_bindings(module: &mut ast::Module) -> Result<SourceBindings> {
    GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let mut names = IdentifierNames::default();
        module.visit_with(&mut names);
        ensure!(
            !names.0.contains(super::decoder::DECODER_TYPE),
            "Reserved compiler type name in source"
        );
        let mut bindings = HashMap::new();
        for item in &module.body {
            let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
                continue;
            };
            if import.type_only {
                continue;
            }
            let namespace = match import.src.value.as_str() {
                Some("perry:clocks") => CapabilityNamespace::Clocks,
                Some("perry:random") => CapabilityNamespace::Random,
                Some("perry:stdio") => CapabilityNamespace::Stdio,
                Some("fs" | "node:fs") => CapabilityNamespace::Filesystem,
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
                        if matches!(namespace, CapabilityNamespace::Filesystem) =>
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
            unresolved: SyntaxContext::empty().apply_mark(unresolved),
            names: names.0,
            shadow_names: HashMap::new(),
            operations: BTreeMap::new(),
            decoder_constructor: None,
            error: None,
        };
        module.visit_mut_with(&mut calls);
        if let Some(error) = calls.error {
            return Err(error);
        }
        let mut resolved = SourceBindings {
            decoder_constructor: calls.decoder_constructor,
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
        if let Some(name) = &resolved.decoder_constructor {
            let declaration = format!(
                "declare function {name}(label: any, options: any): {};",
                super::decoder::DECODER_TYPE
            );
            module
                .body
                .append(&mut parse_typescript(&declaration, "decoder.d.ts")?.body);
        }
        Ok(resolved)
    })
}

fn source_type(ty: &HirType) -> Result<String> {
    match ty {
        ty if super::text_or_bytes::is_text_or_bytes(ty) => Ok("string | Uint8Array".into()),
        HirType::Number => Ok("number".into()),
        HirType::Boolean => Ok("boolean".into()),
        HirType::String => Ok("string".into()),
        HirType::Void => Ok("void".into()),
        HirType::Any => Ok("any".into()),
        HirType::Promise(inner) => Ok(format!("Promise<{}>", source_type(inner)?)),
        ty if super::bytes::is_byte_view(ty) => Ok("Uint8Array".into()),
        _ => bail!("Unsupported capability source type: {ty:?}"),
    }
}

#[derive(Clone, Copy)]
enum CapabilityNamespace {
    Clocks,
    Random,
    Stdio,
    Filesystem,
}

impl CapabilityNamespace {
    fn operation(self, name: &str) -> Result<CapabilityOperation> {
        match (self, name) {
            (Self::Clocks, "waitFor") => Ok(CapabilityOperation::Clock(ClockOperation::WaitFor)),
            (Self::Random, "randomNumber") => {
                Ok(CapabilityOperation::Random(RandomOperation::Number))
            }
            (Self::Stdio, "writeStdout") => {
                Ok(CapabilityOperation::Stdio(StdioOperation::WriteStdout))
            }
            (Self::Stdio, "writeStderr") => {
                Ok(CapabilityOperation::Stdio(StdioOperation::WriteStderr))
            }
            (Self::Filesystem, "writeFileSync") => Ok(CapabilityOperation::Filesystem(
                FilesystemOperation::WriteFile,
            )),
            (Self::Filesystem, "readFileSync") => Ok(CapabilityOperation::Filesystem(
                FilesystemOperation::ReadBytes,
            )),
            _ => bail!("Unknown capability member '{name}'"),
        }
    }
}

enum CapabilityBinding {
    Operation(CapabilityOperation),
    Namespace(CapabilityNamespace),
}

struct SourceCalls {
    bindings: HashMap<ast::Id, CapabilityBinding>,
    unresolved: SyntaxContext,
    names: HashSet<String>,
    shadow_names: HashMap<ast::Id, String>,
    operations: BTreeMap<CapabilityOperation, String>,
    decoder_constructor: Option<String>,
    error: Option<anyhow::Error>,
}

impl SourceCalls {
    fn validate_json_call(&self, call: &ast::CallExpr, callee: &ast::Expr) -> Result<()> {
        let ast::Expr::Member(member) = callee else {
            return Ok(());
        };
        if !matches!(member.obj.as_ref(), ast::Expr::Ident(name) if name.sym == "JSON" && name.ctxt == self.unresolved)
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
                if namespace.is_none() && !builtin_math && !builtin_console {
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
                if let Some(namespace) = namespace {
                    Ok(Some(namespace.operation(name)?))
                } else if builtin_console {
                    Ok(Some(CapabilityOperation::Stdio(match name {
                        "log" => StdioOperation::Log,
                        "error" | "warn" => StdioOperation::Error,
                        _ => bail!("Unsupported console method '{name}'"),
                    })))
                } else if name == "random" {
                    Ok(Some(CapabilityOperation::Random(RandomOperation::Number)))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
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
    fn visit_mut_call_expr(&mut self, call: &mut ast::CallExpr) {
        call.ctxt = SyntaxContext::empty();
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
                Ok(Some(operation)) => {
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
                    let name = if let Some(name) = self.operations.get(&operation) {
                        name.clone()
                    } else {
                        let name = self.fresh_name();
                        self.operations.insert(operation, name.clone());
                        name
                    };
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
                self.error.get_or_insert_with(|| anyhow::anyhow!("Uint8Array construction supports one non-spread argument; backing-buffer overloads are unsupported"));
                return;
            }
        }
        if !self.reject_regexp_constructor(&expression.callee) {
            expression.visit_mut_children_with(self);
        }
    }

    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
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
        if matches!(
            ident.sym.as_ref(),
            "Math" | "RegExp" | "JSON" | "Uint8Array" | "TextDecoder" | "console"
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
            && name.sym == "TextDecoder"
            && name.ctxt == self.unresolved
        {
            name.sym = super::decoder::DECODER_TYPE.into();
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
