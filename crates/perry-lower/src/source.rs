//! Preserve builtin types and literal options before HIR lowering can discard their semantics.
use anyhow::{Result, bail, ensure};
use perry_parser::swc_ecma_ast as ast;
use std::collections::{HashMap, HashSet};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

pub(super) const FS_STATS: &str = "__perry_internal_fs_stats";
pub(super) const ENCODER: &str = "__perry_internal_text_encoder";
pub(super) const DECODER: &str = "__perry_internal_text_decoder";

fn underlying(mut expression: &ast::Expr) -> &ast::Expr {
    loop {
        expression = match expression {
            ast::Expr::Paren(value) => &value.expr,
            ast::Expr::TsAs(value) => &value.expr,
            ast::Expr::TsSatisfies(value) => &value.expr,
            ast::Expr::TsNonNull(value) => &value.expr,
            _ => return expression,
        };
    }
}

fn property(property: &ast::MemberProp) -> Option<&str> {
    match property {
        ast::MemberProp::Ident(name) => Some(name.sym.as_ref()),
        ast::MemberProp::Computed(key) => match underlying(&key.expr) {
            ast::Expr::Lit(ast::Lit::Str(name)) => name.value.as_str(),
            _ => None,
        },
        _ => None,
    }
}

fn options(expression: &ast::Expr) -> Result<()> {
    match underlying(expression) {
        ast::Expr::Ident(name) if name.sym == "undefined" => Ok(()),
        ast::Expr::Lit(ast::Lit::Null(_)) => Ok(()),
        ast::Expr::Object(object) => {
            for entry in &object.props {
                let ast::PropOrSpread::Prop(entry) = entry else {
                    bail!("builtin options do not support spread");
                };
                let name = match entry.as_ref() {
                    ast::Prop::KeyValue(entry) => match &entry.key {
                        ast::PropName::Ident(name) => Some(name.sym.as_ref()),
                        ast::PropName::Str(name) => name.value.as_str(),
                        _ => None,
                    },
                    ast::Prop::Shorthand(name) => Some(name.sym.as_ref()),
                    _ => None,
                };
                ensure!(
                    name.is_some_and(|name| name != "__proto__"),
                    "builtin options require plain named data properties"
                );
            }
            Ok(())
        }
        _ => bail!("builtin options must be a literal object, null or undefined"),
    }
}

pub(super) fn prepare(module: &mut ast::Module) -> Result<()> {
    #[derive(Default)]
    struct Names {
        reserved: bool,
        codecs: bool,
    }
    impl Visit for Names {
        fn visit_ident(&mut self, name: &ast::Ident) {
            self.reserved |= matches!(name.sym.as_ref(), ENCODER | DECODER | FS_STATS);
            self.codecs |= matches!(name.sym.as_ref(), "TextEncoder" | "TextDecoder");
        }
        fn visit_ident_name(&mut self, name: &ast::IdentName) {
            self.codecs |= matches!(name.sym.as_ref(), "TextEncoder" | "TextDecoder");
        }
        fn visit_str(&mut self, name: &ast::Str) {
            self.codecs |= matches!(name.value.as_str(), Some("TextEncoder" | "TextDecoder"));
        }
    }
    let mut names = Names::default();
    module.visit_with(&mut names);
    ensure!(!names.reserved, "reserved compiler builtin name in source");
    let has_fs = module.body.iter().any(|item| matches!(item, ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) if matches!(import.src.value.as_str(), Some("fs" | "node:fs" | "fs/promises" | "node:fs/promises"))));
    if !names.codecs && !has_fs {
        return Ok(());
    }
    if names.codecs {
        let mut declarations = perry_parser::parse_typescript(
            &format!("declare class {ENCODER} {{}} declare class {DECODER} {{}}"),
            "codec-types.ts",
        )?;
        module.body.append(&mut declarations.body);
    }
    GLOBALS.set(&Globals::new(), || {
        let unresolved = Mark::new();
        module.visit_mut_with(&mut resolver(unresolved, Mark::new(), true));
        let mut filesystem = HashMap::new();
        let mut stats = HashSet::new();
        for item in &module.body {
            let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item else {
                continue;
            };
            match import.src.value.as_str() {
                Some("fs/promises" | "node:fs/promises") => {
                    for specifier in &import.specifiers {
                        match specifier {
                            ast::ImportSpecifier::Named(named) => {
                                let name = match &named.imported {
                                    Some(ast::ModuleExportName::Ident(name)) => {
                                        name.sym.to_string()
                                    }
                                    Some(ast::ModuleExportName::Str(name)) => {
                                        name.value.to_string_lossy().into_owned()
                                    }
                                    None => named.local.sym.to_string(),
                                };
                                filesystem.insert(named.local.to_id(), Some(name));
                            }
                            ast::ImportSpecifier::Default(default) => {
                                filesystem.insert(default.local.to_id(), None);
                            }
                            ast::ImportSpecifier::Namespace(namespace) => {
                                filesystem.insert(namespace.local.to_id(), None);
                            }
                        }
                    }
                }
                Some("fs" | "node:fs") => {
                    for specifier in &import.specifiers {
                        if let ast::ImportSpecifier::Named(named) = specifier {
                            let name = match &named.imported {
                                Some(ast::ModuleExportName::Ident(name)) => name.sym.as_ref(),
                                None => named.local.sym.as_ref(),
                                _ => continue,
                            };
                            if name == "Stats" {
                                stats.insert(named.local.to_id());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        let mut rewrite = Builtins {
            filesystem,
            stats,
            unresolved: SyntaxContext::empty().apply_mark(unresolved),
            error: None,
        };
        module.visit_mut_with(&mut rewrite);
        match rewrite.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    })
}

struct Builtins {
    filesystem: HashMap<ast::Id, Option<String>>,
    stats: HashSet<ast::Id>,
    unresolved: SyntaxContext,
    error: Option<anyhow::Error>,
}
impl Builtins {
    fn constructor(&mut self, expression: &mut ast::NewExpr) -> Result<()> {
        let name = match underlying(&expression.callee) {
            ast::Expr::Ident(name) if name.ctxt == self.unresolved => Some(name.sym.as_ref()),
            ast::Expr::Member(member) if matches!(underlying(&member.obj), ast::Expr::Ident(name) if name.sym == "globalThis" && name.ctxt == self.unresolved) => {
                property(&member.prop)
            }
            _ => None,
        };
        let internal = match name {
            Some("TextEncoder") => ENCODER,
            Some("TextDecoder") => DECODER,
            _ => return Ok(()),
        };
        let arguments = expression.args.as_deref().unwrap_or_default();
        ensure!(
            expression.type_args.is_none() && arguments.iter().all(|arg| arg.spread.is_none()),
            "codec constructors do not support type arguments or spread"
        );
        ensure!(
            arguments.len() <= if internal == DECODER { 2 } else { 0 },
            "unsupported codec constructor arguments"
        );
        if let Some(argument) = arguments.get(1) {
            options(&argument.expr)?;
        }
        *expression.callee = ast::Expr::Ident(ast::Ident::new(
            internal.into(),
            expression.span,
            self.unresolved,
        ));
        Ok(())
    }
}
impl VisitMut for Builtins {
    fn visit_mut_new_expr(&mut self, expression: &mut ast::NewExpr) {
        if let Err(error) = self.constructor(expression) {
            self.error.get_or_insert(error);
        }
        expression.visit_mut_children_with(self);
    }
    fn visit_mut_ts_type_ref(&mut self, reference: &mut ast::TsTypeRef) {
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && self.stats.contains(&name.to_id())
        {
            name.sym = FS_STATS.into();
        }
        if let ast::TsEntityName::Ident(name) = &mut reference.type_name
            && name.ctxt == self.unresolved
        {
            match name.sym.as_ref() {
                "TextEncoder" => name.sym = ENCODER.into(),
                "TextDecoder" => name.sym = DECODER.into(),
                _ => {}
            }
        }
        reference.visit_mut_children_with(self);
    }
    fn visit_mut_call_expr(&mut self, expression: &mut ast::CallExpr) {
        if let ast::Callee::Expr(callee) = &expression.callee {
            let operation = match underlying(callee) {
                ast::Expr::Ident(name) => self
                    .filesystem
                    .get(&name.to_id())
                    .and_then(Option::as_deref),
                ast::Expr::Member(member) => match underlying(&member.obj) {
                    ast::Expr::Ident(name) if self.filesystem.get(&name.to_id()) == Some(&None) => {
                        property(&member.prop)
                    }
                    _ => None,
                },
                _ => None,
            };
            if matches!(
                operation,
                Some("stat" | "mkdir" | "unlink" | "rmdir" | "readFile" | "writeFile" | "readdir")
            ) {
                let result = (|| {
                    let position = if operation == Some("writeFile") { 2 } else { 1 };
                    ensure!(
                        expression.args.len() <= position + 1
                            && expression.args.iter().all(|arg| arg.spread.is_none()),
                        "filesystem operation takes a path and options without spread"
                    );
                    if let Some(argument) = expression.args.get(position) {
                        let transfer =
                            matches!(operation, Some("readFile" | "writeFile" | "readdir"));
                        if !transfer || matches!(underlying(&argument.expr), ast::Expr::Object(_)) {
                            options(&argument.expr)?;
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result {
                    self.error.get_or_insert(error);
                }
            }
        }

        if let ast::Callee::Expr(callee) = &expression.callee
            && let ast::Expr::Member(member) = underlying(callee)
            && property(&member.prop) == Some("decode")
        {
            let result = (|| {
                ensure!(
                    expression.args.len() <= 2
                        && expression.args.iter().all(|arg| arg.spread.is_none()),
                    "TextDecoder.decode accepts input and options without spread"
                );
                if let Some(argument) = expression.args.get(1) {
                    options(&argument.expr)?;
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.error.get_or_insert(error);
            }
        }
        expression.visit_mut_children_with(self);
    }
}
