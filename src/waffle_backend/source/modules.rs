//! Resolve a static local module graph before capability and WIT binding.

use anyhow::{Context, Result, bail, ensure};
use perry_parser::{parse_typescript, swc_ecma_ast as ast};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    path::{Path, PathBuf},
};
use swc_common::{DUMMY_SP, GLOBALS, Globals, Mark, SyntaxContext};
use swc_ecma_transforms_base::resolver;
use swc_ecma_visit::{VisitMut, VisitMutWith};

type Exports = BTreeMap<String, String>;

pub(crate) fn load(source: &str, file: &str) -> Result<ast::Module> {
    let initial = parse_typescript(source, file).map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let needs_graph = initial.body.iter().any(|item| match item {
        ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
            local(&import.src.value.to_string_lossy())
        }
        ast::ModuleItem::ModuleDecl(
            ast::ModuleDecl::ExportNamed(_) | ast::ModuleDecl::ExportAll(_),
        ) => true,
        _ => false,
    });
    if !needs_graph {
        return Ok(initial);
    }
    GLOBALS.set(&Globals::new(), || {
        let mut graph = Graph::default();
        let path = std::path::absolute(file)?;
        let path = if path.exists() {
            path.canonicalize()?
        } else {
            path
        };
        let exports = graph.module(&path, initial)?;
        for (name, binding) in exports {
            if graph.types.contains(&binding) {
                continue;
            }
            let mut function = graph
                .functions
                .get(&binding)
                .with_context(|| {
                    format!("Component export '{name}' requires a named function declaration")
                })?
                .clone();
            function.ident = ast::Ident::new(name.into(), DUMMY_SP, SyntaxContext::empty());
            graph
                .body
                .push(ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(
                    ast::ExportDecl {
                        span: DUMMY_SP,
                        decl: ast::Decl::Fn(function),
                    },
                )));
        }
        let mut module = ast::Module {
            body: graph.body,
            ..Default::default()
        };
        module.visit_mut_with(&mut ClearContexts);
        Ok(module)
    })
}

fn local(name: &str) -> bool {
    name.starts_with("./") || name.starts_with("../")
}

fn resolve(parent: &Path, name: &str) -> Result<PathBuf> {
    ensure!(local(name), "Re-exports require a local TypeScript module");
    let path = parent.parent().unwrap_or(Path::new(".")).join(name);
    let mut candidates = vec![path.clone()];
    if path.extension().is_none() {
        candidates.extend([
            path.with_extension("ts"),
            path.with_extension("d.ts"),
            path.join("index.ts"),
        ]);
    }
    if path.extension().is_some_and(|extension| extension == "js") {
        candidates.push(path.with_extension("ts"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .with_context(|| format!("Cannot resolve '{name}' from {}", parent.display()))?
        .canonicalize()
        .map_err(Into::into)
}

#[derive(Default)]
struct Graph {
    loaded: BTreeMap<PathBuf, Exports>,
    active: BTreeSet<PathBuf>,
    body: Vec<ast::ModuleItem>,
    functions: BTreeMap<String, ast::FnDecl>,
    types: BTreeSet<String>,
    next: usize,
}
impl Graph {
    fn dependency(&mut self, parent: &Path, source: &ast::Str) -> Result<Exports> {
        let path = resolve(parent, &source.value.to_string_lossy())?;
        if let Some(exports) = self.loaded.get(&path) {
            return Ok(exports.clone());
        }
        ensure!(
            !self.active.contains(&path),
            "Cyclic local module imports are unsupported: {}",
            path.display()
        );
        let source =
            fs::read_to_string(&path).with_context(|| format!("Reading {}", path.display()))?;
        let module = parse_typescript(&source, &path.to_string_lossy())
            .map_err(|error| anyhow::anyhow!("{error:?}"))?;
        self.module(&path, module)
    }
    fn module(&mut self, path: &Path, mut module: ast::Module) -> Result<Exports> {
        self.active.insert(path.to_owned());
        if path.to_string_lossy().ends_with(".d.ts") {
            module.body.retain(|item| {
                matches!(
                    item,
                    ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(ast::ExportDecl {
                        decl: ast::Decl::TsInterface(_) | ast::Decl::TsTypeAlias(_),
                        ..
                    })) | ast::ModuleItem::Stmt(ast::Stmt::Decl(
                        ast::Decl::TsInterface(_) | ast::Decl::TsTypeAlias(_)
                    ))
                )
            });
        }
        let index = self.next;
        self.next += 1;
        module.visit_mut_with(&mut resolver(Mark::new(), Mark::new(), true));
        let mut names = HashMap::new();
        let mut namespaces = HashMap::new();
        let mut exports = Exports::new();
        for item in &module.body {
            let declaration = match item {
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                    Some(&export.decl)
                }
                ast::ModuleItem::Stmt(ast::Stmt::Decl(decl)) => Some(decl),
                _ => None,
            };
            if let Some(decl) = declaration {
                for id in declared(decl)? {
                    ensure!(
                        !id.sym.starts_with("__perry_"),
                        "Reserved compiler name in source"
                    );
                    names.insert(id.to_id(), format!("__perry_module_{index}_{}", id.sym));
                }
            }
        }
        for item in &mut module.body {
            if let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item {
                if local(&import.src.value.to_string_lossy()) {
                    let dependency = self.dependency(path, &import.src)?;
                    for specifier in &import.specifiers {
                        match specifier {
                            ast::ImportSpecifier::Named(named) => {
                                let original = named
                                    .imported
                                    .as_ref()
                                    .map(|name| name.atom().to_string())
                                    .unwrap_or_else(|| named.local.sym.to_string());
                                names.insert(
                                    named.local.to_id(),
                                    dependency
                                        .get(&original)
                                        .with_context(|| {
                                            format!(
                                                "Module '{}' has no export '{original}'",
                                                import.src.value.to_string_lossy()
                                            )
                                        })?
                                        .clone(),
                                );
                            }
                            ast::ImportSpecifier::Namespace(namespace) => {
                                namespaces.insert(namespace.local.to_id(), dependency.clone());
                            }
                            _ => bail!("Local modules require named imports"),
                        }
                    }
                } else {
                    for specifier in &mut import.specifiers {
                        if let ast::ImportSpecifier::Named(named) = specifier {
                            named.imported.get_or_insert_with(|| {
                                ast::ModuleExportName::Ident(named.local.clone())
                            });
                        }
                        let binding = specifier.local();
                        names.insert(
                            binding.to_id(),
                            format!("__perry_module_{index}_{}", binding.sym),
                        );
                    }
                }
            }
        }
        for item in &module.body {
            match item {
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                    for id in declared(&export.decl)? {
                        insert_export(
                            &mut exports,
                            id.sym.to_string(),
                            names[&id.to_id()].clone(),
                        )?;
                    }
                }
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportNamed(export)) => {
                    let dependency = export
                        .src
                        .as_ref()
                        .map(|source| self.dependency(path, source))
                        .transpose()?;
                    for specifier in &export.specifiers {
                        let ast::ExportSpecifier::Named(named) = specifier else {
                            bail!("Only named local re-exports are supported");
                        };
                        let original = named.orig.atom().to_string();
                        let binding = if let Some(dependency) = &dependency {
                            dependency.get(&original)
                        } else if let ast::ModuleExportName::Ident(id) = &named.orig {
                            names.get(&id.to_id())
                        } else {
                            None
                        }
                        .with_context(|| format!("Unknown export '{original}'"))?;
                        let exported = named
                            .exported
                            .as_ref()
                            .map(|name| name.atom().to_string())
                            .unwrap_or(original);
                        insert_export(&mut exports, exported, binding.clone())?;
                    }
                }
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportAll(export)) => {
                    for (name, binding) in self.dependency(path, &export.src)? {
                        insert_export(&mut exports, name, binding)?;
                    }
                }
                ast::ModuleItem::ModuleDecl(
                    ast::ModuleDecl::ExportDefaultDecl(_) | ast::ModuleDecl::ExportDefaultExpr(_),
                ) => bail!("Component modules use named function exports, not default exports"),
                _ => {}
            }
        }
        let mut renamer = Rename {
            names,
            namespaces,
            error: None,
        };
        for item in module.body {
            let mut item = match item {
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import))
                    if local(&import.src.value.to_string_lossy()) =>
                {
                    continue;
                }
                ast::ModuleItem::ModuleDecl(
                    ast::ModuleDecl::ExportNamed(_) | ast::ModuleDecl::ExportAll(_),
                ) => continue,
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                    ast::ModuleItem::Stmt(ast::Stmt::Decl(export.decl))
                }
                item => item,
            };
            item.visit_mut_with(&mut renamer);
            match &item {
                ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(function))) => {
                    self.functions
                        .insert(function.ident.sym.to_string(), function.clone());
                }
                ast::ModuleItem::Stmt(ast::Stmt::Decl(
                    decl @ (ast::Decl::TsInterface(_) | ast::Decl::TsTypeAlias(_)),
                )) => {
                    self.types
                        .extend(declared(decl)?.into_iter().map(|id| id.sym.to_string()));
                }
                _ => {}
            }
            self.body.push(item);
        }
        if let Some(error) = renamer.error {
            return Err(error);
        }
        self.active.remove(path);
        self.loaded.insert(path.to_owned(), exports.clone());
        Ok(exports)
    }
}
fn insert_export(exports: &mut Exports, name: String, binding: String) -> Result<()> {
    ensure!(
        exports.insert(name.clone(), binding).is_none(),
        "Ambiguous export '{name}'"
    );
    Ok(())
}
fn declared(decl: &ast::Decl) -> Result<Vec<&ast::Ident>> {
    Ok(match decl {
        ast::Decl::Fn(function) => vec![&function.ident],
        ast::Decl::TsInterface(interface) => vec![&interface.id],
        ast::Decl::TsTypeAlias(alias) => vec![&alias.id],
        ast::Decl::Var(var) => var
            .decls
            .iter()
            .map(|decl| {
                let ast::Pat::Ident(id) = &decl.name else {
                    bail!("Module bindings require named identifiers");
                };
                Ok(&id.id)
            })
            .collect::<Result<Vec<_>>>()?,
        _ => bail!("Static modules support functions, type declarations, and constant bindings"),
    })
}
struct Rename {
    names: HashMap<ast::Id, String>,
    namespaces: HashMap<ast::Id, Exports>,
    error: Option<anyhow::Error>,
}
impl VisitMut for Rename {
    fn visit_mut_import_named_specifier(&mut self, import: &mut ast::ImportNamedSpecifier) {
        import.local.visit_mut_with(self);
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
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if let ast::Expr::Member(member) = expression
            && let ast::Expr::Ident(namespace) = member.obj.as_ref()
            && let Some(exports) = self.namespaces.get(&namespace.to_id())
        {
            let name = match &member.prop {
                ast::MemberProp::Ident(id) => Some(id.sym.to_string()),
                _ => None,
            };
            if let Some(binding) = name.and_then(|name| exports.get(&name)) {
                *expression = ast::Expr::Ident(ast::Ident::new(
                    binding.clone().into(),
                    DUMMY_SP,
                    SyntaxContext::empty(),
                ));
            } else {
                self.error.get_or_insert_with(|| {
                    anyhow::anyhow!("Local namespaces require a declared static member")
                });
            }
            return;
        }
        expression.visit_mut_children_with(self);
    }
    fn visit_mut_ident(&mut self, id: &mut ast::Ident) {
        if let Some(name) = self.names.get(&id.to_id()) {
            id.sym = name.clone().into();
        } else if self.namespaces.contains_key(&id.to_id()) {
            self.error.get_or_insert_with(|| {
                anyhow::anyhow!("Local namespaces may only be used for static member access")
            });
        }
    }
}
struct ClearContexts;
impl VisitMut for ClearContexts {
    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }
}
