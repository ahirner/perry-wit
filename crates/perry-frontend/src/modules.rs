//! Resolve a static local module graph before capability and WIT binding.

use crate::hir_compat::{Locations, parse, pattern_bindings};
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{
    Diagnostic, Result, bail_diag, bail_unsupported, codes, diag, ensure_unsupported, unsupported,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
};
use swc_common::{GLOBALS, Globals, Mark, SyntaxContext, util::take::Take};
use swc_ecma_transforms_base::{hygiene::hygiene, resolver};
use swc_ecma_visit::{Visit, VisitMut, VisitMutWith, VisitWith};

type Exports = BTreeMap<String, ast::Ident>;

pub(crate) struct Loaded<'s> {
    pub(crate) module: ast::Module,
    pub(crate) locations: crate::hir_compat::Locations<'s>,
}

pub(crate) fn load<'s>(
    source: &'s str,
    file: &str,
    sources: &'s BTreeMap<String, String>,
    foreign: &HashMap<String, crate::hir::Foreign>,
) -> Result<Loaded<'s>> {
    let mut initial = parse(source, file)?;
    wit_imports(&mut initial, foreign, Path::new(file), sources);
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
        let mut locations = crate::hir_compat::Locations::default();
        locations.add(PathBuf::from(file), source)?;
        return Ok(Loaded {
            module: initial,
            locations,
        });
    }
    GLOBALS.set(&Globals::new(), || {
        let mut graph = Graph {
            foreign,
            sources,
            loaded: BTreeMap::new(),
            active: BTreeSet::new(),
            body: Vec::new(),
            types: HashSet::new(),
            next: 0,
            locations: crate::hir_compat::Locations::default(),
            entry_path: normalize(Path::new(file)),
            entry_source: source,
        };
        let path = normalize(Path::new(file));
        let exports = graph.module(&path, initial)?;
        // An exported function that nothing else names is renamed in place; one that the
        // graph also calls, or exports twice, keeps its binding and gets an exported copy.
        let mut bindings = ExportBindings::default();
        for (name, binding) in &exports {
            if !graph.types.contains(&binding.to_id()) {
                bindings
                    .0
                    .entry(binding.to_id())
                    .or_default()
                    .names
                    .push(name);
            }
        }
        graph.body.visit_with(&mut bindings);
        let mut copies = BTreeMap::new();
        for item in graph.body.iter_mut().rev() {
            if let ast::ModuleItem::Stmt(ast::Stmt::Decl(ast::Decl::Fn(function))) = item
                && let Some(binding) = bindings.0.remove(&function.ident.to_id())
            {
                if let [name] = binding.names.as_slice()
                    && binding.occurrences == 1
                {
                    *item = export_function(function.take(), name);
                } else {
                    for name in binding.names {
                        copies.insert(name, export_function(function.clone(), name));
                    }
                }
            }
        }
        if let Some((name, declaration)) = exports
            .iter()
            .find(|(_, declaration)| bindings.0.contains_key(&declaration.to_id()))
        {
            return Err(graph.locations.annotate(
                unsupported!("Component export '{name}' requires a named function declaration"),
                declaration.span,
            ));
        }
        graph.body.extend(copies.into_values());
        let mut module = ast::Module {
            body: graph.body,
            ..Default::default()
        };
        // Preserve lexical references when the next resolver reads the flattened module.
        module.visit_mut_with(&mut hygiene());
        module.visit_mut_with(&mut ClearContexts);
        Ok(Loaded {
            module,
            locations: graph.locations,
        })
    })
}

/// SDK types are supplied from the selected WIT world, including in-memory test sources.
fn wit_imports(
    module: &mut ast::Module,
    foreign: &HashMap<String, crate::hir::Foreign>,
    parent: &Path,
    sources: &BTreeMap<String, String>,
) {
    for item in &mut module.body {
        if let ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) = item
            && local(&import.src.value.to_string_lossy())
            && !import.specifiers.is_empty()
            && import.specifiers.iter().all(|specifier| match specifier {
                ast::ImportSpecifier::Named(named) => {
                    let imported = named.imported.as_ref().map(ast::ModuleExportName::atom);
                    let name: &str = imported.as_deref().unwrap_or(&named.local.sym);
                    (import.type_only || named.is_type_only)
                        && (foreign
                            .get(name)
                            .is_some_and(|item| !matches!(item, crate::hir::Foreign::Function(_)))
                            || matches!(name, "WitInput" | "WitResource"))
                }
                _ => false,
            })
            && resolve(parent, &import.src.value.to_string_lossy(), sources).map_or(true, |path| {
                path.to_string_lossy().ends_with(".d.ts")
                    && sources
                        .get(path.to_string_lossy().as_ref())
                        .is_some_and(|source| {
                            source.contains("Generated by Perry-WIT SDK compiler.")
                        })
            })
        {
            import.src.value = "perry:wit-types".into();
        }
    }
}

fn local(name: &str) -> bool {
    name.starts_with("./") || name.starts_with("../")
}

pub(crate) fn candidates(parent: &Path, name: &str) -> Result<Vec<PathBuf>> {
    ensure_unsupported!(local(name), "Re-exports require a local TypeScript module");
    let path = normalize(&parent.parent().unwrap_or(Path::new("")).join(name));
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
    Ok(candidates)
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir
                if result.file_name().is_some_and(|name| name != "..") =>
            {
                result.pop();
            }
            _ => result.push(part),
        }
    }
    result
}

fn resolve(parent: &Path, name: &str, sources: &BTreeMap<String, String>) -> Result<PathBuf> {
    candidates(parent, name)?
        .into_iter()
        .find(|path| sources.contains_key(path.to_string_lossy().as_ref()))
        .ok_or_else(|| {
            Diagnostic::new(
                codes::UNRESOLVED_MODULE,
                format!("Cannot resolve '{name}' from {}", parent.display()),
            )
        })
}

pub(crate) fn imports(module: &ast::Module) -> impl Iterator<Item = String> + '_ {
    module
        .body
        .iter()
        .filter_map(|item| match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => Some(&import.src),
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportNamed(export)) => {
                export.src.as_ref()
            }
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportAll(export)) => Some(&export.src),
            _ => None,
        })
        .map(|source| source.value.to_string_lossy().into_owned())
        .filter(|name| local(name))
}

struct Graph<'a, 'f> {
    sources: &'a BTreeMap<String, String>,
    foreign: &'f HashMap<String, crate::hir::Foreign>,
    loaded: BTreeMap<PathBuf, Exports>,
    active: BTreeSet<PathBuf>,
    body: Vec<ast::ModuleItem>,
    types: HashSet<ast::Id>,
    next: usize,
    locations: crate::hir_compat::Locations<'a>,
    entry_path: PathBuf,
    entry_source: &'a str,
}
impl Graph<'_, '_> {
    fn dependency(&mut self, parent: &Path, source: &ast::Str) -> Result<PathBuf> {
        let path = resolve(parent, &source.value.to_string_lossy(), self.sources)
            .map_err(|error| self.locations.annotate(error, source.span))?;
        if self.loaded.contains_key(&path) {
            return Ok(path);
        }
        if self.active.contains(&path) {
            return Err(self.locations.annotate(
                diag!(
                    codes::CYCLIC_IMPORT,
                    "Cyclic local module imports are unsupported: {}",
                    path.display()
                ),
                source.span,
            ));
        }
        let source = &self.sources[path.to_string_lossy().as_ref()];
        let module = parse(source, &path.to_string_lossy())?;
        let exports = self.module(&path, module)?;
        self.loaded.insert(path.clone(), exports);
        Ok(path)
    }
    fn module(&mut self, path: &Path, mut module: ast::Module) -> Result<Exports> {
        wit_imports(&mut module, self.foreign, path, self.sources);
        // Each parser invocation owns a source map; give merged bindings disjoint spans.
        struct Rebase(u32);
        impl VisitMut for Rebase {
            fn visit_mut_span(&mut self, span: &mut swc_common::Span) {
                if !span.is_dummy() {
                    span.lo.0 += self.0;
                    span.hi.0 += self.0;
                }
            }
        }
        let text = if path == self.entry_path {
            self.entry_source
        } else {
            &self.sources[path.to_string_lossy().as_ref()]
        };
        let offset = self.locations.add(path.to_owned(), text)?;
        module.visit_mut_with(&mut Rebase(offset));
        crate::engine::boundary::scan_ast(&module, &self.locations)?;
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
        let mut dependencies = HashMap::new();
        for item in &module.body {
            let source = match item {
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import))
                    if local(&import.src.value.to_string_lossy()) =>
                {
                    Some(import.src.as_ref())
                }
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportNamed(export)) => {
                    export.src.as_deref()
                }
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportAll(export)) => {
                    Some(export.src.as_ref())
                }
                _ => None,
            };
            if let Some(source) = source {
                dependencies.insert(&source.value, self.dependency(path, source)?);
            }
        }
        let (mut renamer, exports) = bind_module(&module, index, &self.locations, |source| {
            &self.loaded[&dependencies[&source.value]]
        })?;
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
                ast::ModuleItem::Stmt(ast::Stmt::Decl(
                    decl @ (ast::Decl::TsInterface(_)
                    | ast::Decl::TsTypeAlias(_)
                    | ast::Decl::Class(_)),
                )) => {
                    self.types
                        .extend(declared(decl)?.into_iter().map(ast::Ident::to_id));
                }
                ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
                    for specifier in &import.specifiers {
                        if let ast::ImportSpecifier::Named(named) = specifier
                            && (import.type_only || named.is_type_only)
                        {
                            self.types.insert(named.local.to_id());
                        }
                    }
                }
                _ => {}
            }
            self.body.push(item);
        }
        if let Some((error, span)) = renamer.error {
            return Err(self.locations.annotate(error, span));
        }
        self.active.remove(path);
        Ok(exports)
    }
}
fn bind_module<'a>(
    module: &ast::Module,
    index: usize,
    locations: &Locations<'_>,
    dependency_exports: impl Fn(&ast::Str) -> &'a Exports,
) -> Result<(Rename<'a>, Exports)> {
    let mut names = HashMap::new();
    let mut namespaces = HashMap::new();
    let mut exports = Exports::new();
    let module_binding = |id: &ast::Ident| {
        ast::Ident::new(
            format!("__perry_module_{index}_{}", id.sym).into(),
            id.span,
            id.ctxt,
        )
    };
    for item in &module.body {
        match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(ast::ExportDecl {
                decl,
                ..
            }))
            | ast::ModuleItem::Stmt(ast::Stmt::Decl(decl)) => {
                for id in declared(decl)? {
                    if id.sym.starts_with("__perry_") {
                        return Err(locations
                            .annotate(unsupported!("Reserved compiler name in source"), id.span));
                    }
                    names.insert(id.to_id(), module_binding(id));
                }
            }
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::Import(import)) => {
                if local(&import.src.value.to_string_lossy()) {
                    let dependency = dependency_exports(&import.src);
                    for specifier in &import.specifiers {
                        match specifier {
                            ast::ImportSpecifier::Named(named) => {
                                let imported =
                                    named.imported.as_ref().map(ast::ModuleExportName::atom);
                                let original: &str =
                                    imported.as_deref().unwrap_or(&named.local.sym);
                                names.insert(
                                    named.local.to_id(),
                                    dependency
                                        .get(original)
                                        .ok_or_else(|| {
                                            diag!(
                                                codes::UNKNOWN_EXPORT,
                                                "Module '{}' has no export '{original}'",
                                                import.src.value.to_string_lossy()
                                            )
                                        })?
                                        .clone(),
                                );
                            }
                            ast::ImportSpecifier::Namespace(namespace) => {
                                namespaces.insert(
                                    namespace.local.to_id(),
                                    (import.src.value.to_string_lossy().into_owned(), dependency),
                                );
                            }
                            _ => bail_unsupported!("Local modules require named imports"),
                        }
                    }
                } else {
                    for specifier in &import.specifiers {
                        let binding = specifier.local();
                        names.insert(binding.to_id(), module_binding(binding));
                    }
                }
            }
            _ => {}
        }
    }
    for item in &module.body {
        match item {
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(export)) => {
                for id in declared(&export.decl)? {
                    insert_export(&mut exports, id.sym.to_string(), names[&id.to_id()].clone())?;
                }
            }
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportNamed(export)) => {
                let dependency = export.src.as_ref().map(|source| dependency_exports(source));
                for specifier in &export.specifiers {
                    let ast::ExportSpecifier::Named(named) = specifier else {
                        bail_unsupported!("Only named local re-exports are supported");
                    };
                    let original = named.orig.atom();
                    let original_str: &str = original.as_ref();
                    let binding = if let Some(dependency) = &dependency {
                        dependency.get(original_str)
                    } else if let ast::ModuleExportName::Ident(id) = &named.orig {
                        names.get(&id.to_id())
                    } else {
                        None
                    }
                    .ok_or_else(|| {
                        diag!(codes::UNKNOWN_EXPORT, "Unknown export '{original_str}'")
                    })?;
                    let exported = named
                        .exported
                        .as_ref()
                        .map_or_else(|| original.to_string(), |name| name.atom().to_string());
                    insert_export(&mut exports, exported, binding.clone())?;
                }
            }
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportAll(export)) => {
                for (name, binding) in dependency_exports(&export.src) {
                    insert_export(&mut exports, name.clone(), binding.clone())?;
                }
            }
            ast::ModuleItem::ModuleDecl(
                ast::ModuleDecl::ExportDefaultDecl(_) | ast::ModuleDecl::ExportDefaultExpr(_),
            ) => bail_unsupported!(
                "Component modules use named function exports, not default exports"
            ),
            _ => {}
        }
    }
    Ok((
        Rename {
            names,
            namespaces,
            error: None,
        },
        exports,
    ))
}

fn insert_export(exports: &mut Exports, name: String, binding: ast::Ident) -> Result<()> {
    use std::collections::btree_map::Entry;
    match exports.entry(name) {
        Entry::Vacant(entry) => {
            entry.insert(binding);
            Ok(())
        }
        Entry::Occupied(entry) => {
            bail_diag!(
                codes::AMBIGUOUS_EXPORT,
                "Ambiguous export '{}'",
                entry.key()
            )
        }
    }
}
fn declared(decl: &ast::Decl) -> Result<Vec<&ast::Ident>> {
    Ok(match decl {
        ast::Decl::Fn(function) => vec![&function.ident],
        ast::Decl::Class(class) => vec![&class.ident],
        ast::Decl::TsInterface(interface) => vec![&interface.id],
        ast::Decl::TsTypeAlias(alias) => vec![&alias.id],
        ast::Decl::Var(var) => {
            let mut names = Vec::new();
            for declaration in &var.decls {
                pattern_bindings(&declaration.name, false, &mut |id, _| names.push(id));
            }
            names
        }
        _ => bail_unsupported!(
            "Static modules support functions, type declarations, and constant bindings"
        ),
    })
}
struct Rename<'a> {
    names: HashMap<ast::Id, ast::Ident>,
    namespaces: HashMap<ast::Id, (String, &'a Exports)>,
    error: Option<(Diagnostic, swc_common::Span)>,
}
impl VisitMut for Rename<'_> {
    fn visit_mut_import_named_specifier(&mut self, import: &mut ast::ImportNamedSpecifier) {
        import
            .imported
            .get_or_insert_with(|| ast::ModuleExportName::Ident(import.local.clone()));
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
    fn visit_mut_object_pat_prop(&mut self, prop: &mut ast::ObjectPatProp) {
        if let ast::ObjectPatProp::Assign(assign) = prop
            && self.names.contains_key(&assign.key.to_id())
        {
            let binding = assign.key.take();
            let key = ast::IdentName::new(binding.id.sym.clone(), binding.id.span);
            let value = match assign.value.take() {
                Some(default) => ast::Pat::Assign(ast::AssignPat {
                    span: assign.span,
                    left: Box::new(binding.into()),
                    right: default,
                }),
                None => binding.into(),
            };
            *prop = ast::KeyValuePatProp {
                key: key.into(),
                value: Box::new(value),
            }
            .into();
        }
        prop.visit_mut_children_with(self);
    }
    fn visit_mut_expr(&mut self, expression: &mut ast::Expr) {
        if let ast::Expr::Member(member) = expression
            && let ast::Expr::Ident(namespace) = member.obj.as_ref()
            && let Some((specifier, exports)) = self.namespaces.get(&namespace.to_id())
        {
            match &member.prop {
                ast::MemberProp::Ident(id) => {
                    let name: &str = id.sym.as_ref();
                    if let Some(binding) = exports.get(name) {
                        *expression = ast::Expr::Ident(ast::Ident::new(
                            binding.sym.clone(),
                            member.span,
                            binding.ctxt,
                        ));
                    } else {
                        self.error.get_or_insert_with(|| {
                            (
                                diag!(
                                    codes::UNKNOWN_EXPORT,
                                    "Module '{specifier}' has no export '{name}'"
                                ),
                                member.span,
                            )
                        });
                    }
                }
                _ => {
                    self.error.get_or_insert_with(|| {
                        (
                            unsupported!("Local namespaces require a declared static member"),
                            member.span,
                        )
                    });
                }
            }
            return;
        }
        expression.visit_mut_children_with(self);
    }
    fn visit_mut_ident(&mut self, id: &mut ast::Ident) {
        if let Some(name) = self.names.get(&id.to_id()) {
            id.sym = name.sym.clone();
            id.ctxt = name.ctxt;
        } else if self.namespaces.contains_key(&id.to_id()) {
            self.error.get_or_insert_with(|| {
                (
                    unsupported!("Local namespaces may only be used for static member access"),
                    id.span,
                )
            });
        }
    }
}
fn export_function(mut function: ast::FnDecl, name: &str) -> ast::ModuleItem {
    function.ident.sym = name.into();
    ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(ast::ExportDecl {
        span: function.function.span,
        decl: ast::Decl::Fn(function),
    }))
}

#[derive(Default)]
struct ExportBinding<'a> {
    names: Vec<&'a str>,
    occurrences: usize,
}

#[derive(Default)]
struct ExportBindings<'a>(HashMap<ast::Id, ExportBinding<'a>>);

impl Visit for ExportBindings<'_> {
    fn visit_ident(&mut self, ident: &ast::Ident) {
        if let Some(binding) = self.0.get_mut(&ident.to_id()) {
            binding.occurrences += 1;
        }
    }
}

struct ClearContexts;
impl VisitMut for ClearContexts {
    fn visit_mut_syntax_context(&mut self, context: &mut SyntaxContext) {
        *context = SyntaxContext::empty();
    }
}
