//! Entry-module declarations that implement the selected world's exports.
use super::Locations;
use crate::wit::{ResourceId, WitValue};
use perry_contract::types::Input;
use perry_hir::types::FuncId;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use std::collections::{BTreeMap, HashMap, HashSet};
use swc_common::{Span, Spanned};
use wit_parser::{Resolve, WorldId, WorldKey};

pub(crate) enum ExportDeclaration<'r> {
    Function(perry_contract::WorldFunction<'r>, Vec<Option<Span>>),
    Destructor,
}

pub(crate) struct Signature<'r> {
    pub(crate) declaration: ExportDeclaration<'r>,
    pub(crate) params: Vec<WitValue<'r>>,
    pub(crate) result: Option<WitValue<'r>>,
}

pub(crate) type SourceWitness<'r> = Witness<'r, Target<FuncId>>;
pub(crate) type Witnesses<'r, T> = Vec<Witness<'r, T>>;

pub(crate) struct Witness<'r, T> {
    pub(crate) name: String,
    pub(crate) signature: Option<Signature<'r>>,
    pub(crate) target: Option<T>,
}

impl<'r, T> Witness<'r, T> {
    pub(crate) fn with_target<U>(self, target: Option<U>) -> Witness<'r, U> {
        Witness {
            name: self.name,
            signature: self.signature,
            target,
        }
    }
}

#[derive(Clone, Copy)]
enum Declaration<'a> {
    Function(Span, &'a ast::Function),
    Arrow(&'a ast::ArrowExpr),
    UnsupportedMethod(Span),
    Class(&'a ast::Class),
    Value(&'a ast::Expr),
}

#[derive(Clone, Copy)]
pub(crate) enum Target<F> {
    Function(F),
    Constructor(ResourceId),
    Method(ResourceId, F),
    Static(ResourceId, F),
    Destructor(ResourceId, Option<F>),
}

impl<F> Target<F> {
    pub(crate) fn try_map<G>(self, mut f: impl FnMut(F) -> Result<G>) -> Result<Target<G>> {
        Ok(match self {
            Self::Function(id) => Target::Function(f(id)?),
            Self::Constructor(owner) => Target::Constructor(owner),
            Self::Method(owner, id) => Target::Method(owner, f(id)?),
            Self::Static(owner, id) => Target::Static(owner, f(id)?),
            Self::Destructor(owner, id) => Target::Destructor(owner, id.map(f).transpose()?),
        })
    }
}

pub(crate) type ResourceClasses<T> = BTreeMap<ResourceId, ResourceClass<T>>;

pub(crate) struct ResourceClass<T> {
    pub(crate) class: T,
    pub(crate) binding: ResourceBinding,
}

impl<T> ResourceClass<T> {
    pub(crate) fn with_class<U>(self, class: U) -> ResourceClass<U> {
        ResourceClass {
            class,
            binding: self.binding,
        }
    }
}

pub(crate) struct ResourceBinding {
    name: String,
    pub(crate) create: String,
    pub(crate) representation: String,
}

pub(crate) struct Exports<'r> {
    pub(crate) functions: Vec<Witness<'r, Target<FuncId>>>,
    pub(crate) resources: ResourceClasses<crate::source::ClassId>,
}

pub(crate) fn adapter_name(export: usize) -> String {
    format!("resource_adapter_{export}")
}

#[derive(Default)]
pub(super) struct Roots<'r> {
    namespaces: HashSet<u32>,
    pub(super) witnesses: Vec<Witness<'r, Target<Span>>>,
    classes: ResourceClasses<Span>,
}

impl Declaration<'_> {
    fn function_span(self) -> Option<Span> {
        match self {
            Self::Function(s, _) => Some(s),
            Self::Arrow(a) => Some(a.span),
            _ => None,
        }
    }

    fn annotations(self) -> Vec<Option<Span>> {
        fn annotation(pat: &ast::Pat) -> Option<Span> {
            let ty = match pat {
                ast::Pat::Ident(i) => i.type_ann.as_ref(),
                ast::Pat::Array(a) => a.type_ann.as_ref(),
                ast::Pat::Object(o) => o.type_ann.as_ref(),
                ast::Pat::Rest(r) => r.type_ann.as_ref(),
                ast::Pat::Assign(a) => return annotation(&a.left),
                _ => None,
            };
            ty.map(|t| t.type_ann.span())
        }
        let (mut params, result) = match self {
            Self::Function(_, f) => (
                f.params
                    .iter()
                    .map(|p| annotation(&p.pat))
                    .collect::<Vec<_>>(),
                &f.return_type,
            ),
            Self::Arrow(a) => (a.params.iter().map(annotation).collect(), &a.return_type),
            _ => return Vec::new(),
        };
        params.push(result.as_ref().map(|t| t.type_ann.span()));
        params
    }
}

struct Scope<'a>(HashMap<ast::Id, (&'a ast::Ident, Declaration<'a>)>);

impl<'a> Scope<'a> {
    fn declaration(&self, mut d: Declaration<'a>) -> Option<Declaration<'a>> {
        for _ in 0..=self.0.len() {
            d = match d {
                Declaration::Value(v) => match v.unwrap_parens() {
                    ast::Expr::Fn(f) => Declaration::Function(f.function.span, &f.function),
                    ast::Expr::Arrow(f) => Declaration::Arrow(f),
                    ast::Expr::Ident(id) => self.0.get(&id.to_id())?.1,
                    ast::Expr::Class(c) => Declaration::Class(&c.class),
                    v => return Some(Declaration::Value(v)),
                },
                d => return Some(d),
            };
        }
        None
    }

    fn members(&self, object: &'a ast::ObjectLit) -> HashMap<&'a str, Declaration<'a>> {
        object
            .props
            .iter()
            .filter_map(|p| {
                let ast::PropOrSpread::Prop(p) = p else {
                    return None;
                };
                let (name, decl) = match &**p {
                    ast::Prop::Shorthand(id) => (id.sym.as_str(), self.0.get(&id.to_id())?.1),
                    ast::Prop::KeyValue(kv) => (key(&kv.key)?, Declaration::Value(&kv.value)),
                    ast::Prop::Method(m) => (
                        key(&m.key)?,
                        if m.function.is_async || m.function.is_generator {
                            Declaration::UnsupportedMethod(m.span())
                        } else {
                            Declaration::Function(m.span(), &m.function)
                        },
                    ),
                    _ => return None,
                };
                Some((name, self.declaration(decl)?))
            })
            .collect()
    }
}

pub(super) fn resolve<'a, 'r>(
    module: &'a ast::Module,
    exports: &BTreeMap<String, ast::Ident>,
    world: Option<(&'r Resolve, WorldId)>,
    locations: &mut Locations<'_>,
    unresolved: swc_common::SyntaxContext,
) -> Result<Roots<'r>> {
    use perry_contract::names::{to_camel_case, to_pascal_case};
    let mut scope = Scope(HashMap::new());
    let mut insert = |id: &'a ast::Ident, decl| {
        scope.0.insert(id.to_id(), (id, decl));
    };
    for item in &module.body {
        let d = match item {
            ast::ModuleItem::Stmt(ast::Stmt::Decl(d)) => d,
            ast::ModuleItem::ModuleDecl(ast::ModuleDecl::ExportDecl(d)) => &d.decl,
            _ => continue,
        };
        match d {
            ast::Decl::Fn(f) => insert(
                &f.ident,
                Declaration::Function(f.function.span, &f.function),
            ),
            ast::Decl::Class(c) => insert(&c.ident, Declaration::Class(&c.class)),
            ast::Decl::Var(v) => {
                for d in &v.decls {
                    if let (ast::Pat::Ident(i), Some(e)) = (&d.name, &d.init) {
                        insert(&i.id, Declaration::Value(e));
                    }
                }
            }
            _ => {}
        }
    }
    // Flattening and SWC hygiene preserve declaration spans across resolver contexts.
    let declarations: HashMap<_, _> = scope
        .0
        .values()
        .map(|(id, decl)| (id.span, *decl))
        .collect();
    let exported = |name: &str| {
        let span = exports.get(name)?.span;
        Some((span, scope.declaration(*declarations.get(&span)?)?))
    };
    let mut roots = Roots::default();
    let Some((r, w)) = world else {
        for name in exports.keys() {
            if let Some(span) = exported(name).and_then(|(_, d)| d.function_span()) {
                roots.witnesses.push(Witness {
                    name: name.clone(),
                    signature: None,
                    target: Some(Target::Function(span)),
                });
            }
        }
        roots.witnesses.sort_by_key(|w| match w.target {
            Some(Target::Function(span)) => Some(span.lo),
            _ => None,
        });
        return Ok(roots);
    };
    let world = &r.worlds[w];
    use swc_ecma_visit::{Visit, VisitWith};
    struct Uses<'a>(&'a Scope<'a>, &'a mut HashSet<u32>);
    impl Visit for Uses<'_> {
        fn visit_ident(&mut self, id: &ast::Ident) {
            if let Some((binding, _)) = self.0.0.get(&id.to_id())
                && binding.span != id.span
            {
                self.1.remove(&binding.span.lo.0);
            }
        }
    }
    let mut disposals = Vec::new();
    let mut namespaces = HashMap::new();
    let mut methods = HashMap::new();
    for (k, id) in perry_contract::world_interfaces(&world.exports) {
        let Some((span, Declaration::Value(ast::Expr::Object(obj)))) =
            interface_name(k, r, id).and_then(|name| exported(&to_camel_case(name)))
        else {
            continue;
        };
        let members = namespaces.entry(k).or_insert_with(|| scope.members(obj));
        roots.namespaces.insert(span.lo.0);
        for (name, resource) in perry_contract::interface_resources(r, id) {
            let Some(Declaration::Class(class)) =
                members.get(to_pascal_case(name).as_str()).copied()
            else {
                continue;
            };
            let module = r.name_world_key(k);
            let source_name = perry_contract::names::NamedType::get(r, resource)
                .map(|name| name.qualified())
                .ok_or_else(|| Diagnostic::internal("unnamed exported resource"))?;
            let owner = ResourceId::of(resource);
            let destructor = format!("{source_name}::[Symbol.dispose]");
            roots.classes.insert(
                owner,
                ResourceClass {
                    class: class.span,
                    binding: ResourceBinding {
                        name: source_name,
                        create: format!(
                            "{module}#{}",
                            perry_contract::ResourceOperation::New.name(name)
                        ),
                        representation: format!(
                            "{module}#{}",
                            perry_contract::ResourceOperation::Representation.name(name)
                        ),
                    },
                },
            );
            let mut member = None;
            for m in &class.body {
                let ast::ClassMember::Method(m) = m else {
                    continue;
                };
                if let Some(name) = key(&m.key) {
                    methods.entry((owner, m.is_static, name)).or_insert(m.span);
                    continue;
                }
                if let ast::PropName::Computed(k) = &m.key
                    && let ast::Expr::Member(e) = k.expr.unwrap_parens()
                    && matches!(&*e.obj,ast::Expr::Ident(id) if id.sym=="Symbol" && id.ctxt==unresolved)
                    && matches!(&e.prop,ast::MemberProp::Ident(i) if i.sym=="dispose")
                {
                    member.get_or_insert(m.span);
                }
            }
            disposals.push((
                member.unwrap_or(class.span),
                destructor,
                Witness {
                    name: format!("{module}#[dtor]{name}"),
                    signature: Some(Signature {
                        declaration: ExportDeclaration::Destructor,
                        params: vec![WitValue {
                            resolve: r,
                            ty: wit_parser::Type::U32,
                        }],
                        result: None,
                    }),
                    target: Some(Target::Destructor(owner, member)),
                },
            ));
        }
    }
    module.visit_with(&mut Uses(&scope, &mut roots.namespaces));
    let mut legacy_names = BTreeMap::new();
    for entry in perry_contract::world_functions(r, world.exports.iter()) {
        let f = entry.function;
        let name = entry.implementation_name(r, world);
        let requirement = entry.core_name(r);
        let mut target = None;
        let mut annotations = Vec::new();
        if let Some(owner) = f.kind.resource().map(ResourceId::of)
            && let Some(class) = roots.classes.get(&owner)
        {
            let member = to_camel_case(f.item_name());
            let method = |is_static| methods.get(&(owner, is_static, member.as_str())).copied();
            let declaration = match f.kind {
                wit_parser::FunctionKind::Constructor(_) => {
                    Some((class.class, Target::Constructor(owner)))
                }
                wit_parser::FunctionKind::Static(_) | wit_parser::FunctionKind::AsyncStatic(_) => {
                    method(true).map(|s| (s, Target::Static(owner, s)))
                }
                _ => method(false).map(|s| (s, Target::Method(owner, s))),
            };
            if let Some((span, selected)) = declaration {
                target = Some(selected);
                locations.register_generated(
                    adapter_name(roots.witnesses.len()),
                    span,
                    format!("{}::{member}", class.binding.name),
                );
            }
        } else if let Some(decl) = namespaces
            .get(entry.key)
            .and_then(|members| members.get(to_camel_case(&f.name).as_str()))
            .copied()
        {
            if let Declaration::UnsupportedMethod(span) = decl {
                let error = Diagnostic::unsupported(format!(
                    "async and generator method syntax is unsupported for WIT export {requirement}"
                ))
                .with_help("Export a reference to a function declaration in the interface object.");
                return Err(locations.annotate(error, span));
            }
            target = decl.function_span().map(Target::Function);
            annotations = decl.annotations();
        }
        if target.is_none()
            && let Some((_, decl)) = exported(&name)
            && let Some(span) = decl.function_span()
        {
            if let Some(previous) = legacy_names.insert(name.clone(), requirement.clone()) {
                return Err(locations.annotate(diag!(codes::CONTRACT_MISMATCH,"WIT exports '{previous}' and '{requirement}' both require TypeScript implementation '{name}'"),span));
            }
            target = Some(Target::Function(span));
            annotations = decl.annotations();
        }
        if target.is_none() && !entry.is_command(r) {
            let form = entry
                .interface
                .and_then(|id| interface_name(entry.key, r, id))
                .map(|interface| {
                    format!(
                        "export const {} = {{ {} }}",
                        to_camel_case(interface),
                        to_camel_case(f.item_name())
                    )
                })
                .unwrap_or_else(|| format!("export function {name}"));
            let err = diag!(
                codes::MISSING_EXPORT,
                "WIT export {requirement} needs a source implementation"
            )
            .with_help(format!("Implement the requirement with `{form}`."));
            if let Some((id, _)) = scope
                .0
                .values()
                .find(|(id, _)| super::display_name(&id.sym) == name)
            {
                return Err(locations.annotate(
                    err.with_help(format!("Add the 'export' keyword to function '{name}'.")),
                    id.span,
                ));
            }
            let near = exports.iter().max_by_key(|(e, _)| {
                e.bytes()
                    .zip(name.bytes())
                    .take_while(|(a, b)| a == b)
                    .count()
            });
            return Err(match near {
                Some((near, id)) => locations.annotate(
                    err.with_note(format!("The entry module exports '{near}'.")),
                    id.span,
                ),
                None => err.with_fallback_span(locations.offset_span(1, 1)),
            });
        }
        let ty = |mut ty| {
            while let wit_parser::Type::Id(id) = ty
                && let wit_parser::TypeDefKind::Type(inner) = r.types[id].kind
            {
                ty = inner;
            }
            crate::wit::supported(r, Input::Reference(ty))
                .then_some(WitValue { resolve: r, ty })
                .ok_or_else(|| {
                    Diagnostic::unsupported(format!(
                        "WIT export {requirement} uses an unsupported type"
                    ))
                })
        };
        roots.witnesses.push(Witness {
            name,
            target,
            signature: Some(Signature {
                declaration: ExportDeclaration::Function(entry, annotations),
                params: f.params.iter().map(|p| ty(p.ty)).collect::<Result<_>>()?,
                result: f.result.map(ty).transpose()?,
            }),
        });
    }
    for (span, label, witness) in disposals {
        locations.register_generated(adapter_name(roots.witnesses.len()), span, label);
        roots.witnesses.push(witness);
    }
    Ok(roots)
}

fn interface_name<'a>(
    key: &'a WorldKey,
    r: &'a Resolve,
    id: wit_parser::InterfaceId,
) -> Option<&'a str> {
    match key {
        WorldKey::Name(n) => Some(n),
        WorldKey::Interface(_) => r.interfaces[id].name.as_deref(),
    }
}

fn key(prop: &ast::PropName) -> Option<&str> {
    match prop {
        ast::PropName::Ident(id) => Some(&id.sym),
        ast::PropName::Str(s) => s.value.as_str(),
        _ => None,
    }
}

impl<'r> Roots<'r> {
    pub(super) fn in_hir(
        self,
        module: &mut perry_hir::ir::Module,
        locations: &mut Locations,
    ) -> Result<Exports<'r>> {
        use perry_hir::ir::{Expr, Stmt};
        let reference = |e: &Expr| matches!(e, Expr::FuncRef(_) | Expr::ClassRef(_));
        module.init.retain(|stmt| match stmt {
            Stmt::Let {
                id,
                mutable: false,
                init: Some(init),
                ..
            } if module
                .local_source_spans
                .get(id)
                .is_some_and(|s| self.namespaces.contains(&s.start)) =>
            {
                match init {
                    Expr::Object(fields) => !fields.iter().all(|(_, e)| reference(e)),
                    Expr::New {
                        class_name, args, ..
                    } if super::is_object_literal(class_name) => !args.iter().all(reference),
                    _ => true,
                }
            }
            _ => true,
        });
        let classes: HashMap<_, _> = module
            .classes
            .iter()
            .filter_map(|class| {
                let text = module.class_source_text.get(&class.id)?;
                Some((text.source_range(), crate::source::ClassId::of(class)?))
            })
            .collect();
        let mut resources = BTreeMap::new();
        for (owner, resource) in self.classes {
            let span = resource.class;
            let range = span.lo.0.saturating_sub(1) as usize..span.hi.0.saturating_sub(1) as usize;
            let &class = classes
                .get(&range)
                .ok_or_else(|| Diagnostic::internal("resource class has no HIR declaration"))?;
            resources.insert(owner, resource.with_class(class));
        }

        let declarations: HashMap<_, _> = module
            .closure_source_text
            .iter()
            .filter_map(|(&id, text)| Some((text.source_span?, id)))
            .collect();
        let mut disposals = HashSet::new();
        let mut exported_functions = HashMap::new();
        let mut functions = Vec::with_capacity(self.witnesses.len());
        for w in self.witnesses {
            let id = |span: Span| {
                declarations
                    .get(&(span.lo.0, span.hi.0))
                    .copied()
                    .ok_or_else(|| {
                        Diagnostic::internal(format!("no HIR declaration for export '{}'", w.name))
                    })
            };
            let target = w.target.map(|t| t.try_map(id)).transpose()?;
            if let Some(Target::Destructor(_, Some(id))) = target {
                disposals.insert(id);
            }
            if let (Some(Target::Function(span)), Some(Target::Function(id))) = (w.target, target) {
                exported_functions.insert(id, span);
            }
            functions.push(w.with_target(target));
        }
        for f in &module.functions {
            if let Some(&span) = exported_functions.get(&f.id) {
                locations.register_generated(
                    f.name.clone(),
                    span,
                    super::display_name(&f.name).to_owned(),
                );
            }
        }
        for class in &mut module.classes {
            for member in class
                .computed_members
                .extract_if(.., |m| disposals.remove(&m.function.id))
            {
                let method = member.function;
                module.init.retain(|stmt| !matches!(stmt, Stmt::Expr(Expr::RegisterClassComputedMethod {class_name,method_name,..})
                    if class_name==&class.name && method_name==&method.name));
                class.methods.push(method);
            }
        }
        if !disposals.is_empty() {
            return Err(Diagnostic::internal("missing disposal method"));
        }
        Ok(Exports {
            functions,
            resources,
        })
    }
}
