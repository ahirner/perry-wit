//! Entry-module declarations that implement the selected world's exports.
use super::Locations;
use crate::hir::Type as HirType;
use perry_contract::types::Input;
use perry_hir::types::FuncId;
use perry_parser::swc_ecma_ast as ast;
use perry_wit_diagnostics::{Diagnostic, Result, codes, diag};
use std::collections::{BTreeMap, HashMap};
use swc_common::{Span, Spanned};
use wit_parser::{Resolve, WorldId, WorldKey};

pub(crate) struct Signature {
    pub(super) annotations: Vec<Option<Span>>,
    pub(crate) params: Vec<HirType>,
    pub(crate) result: HirType,
}

pub(crate) struct Witness<T> {
    pub(crate) name: String,
    pub(crate) signature: Option<Signature>,
    pub(crate) target: Option<T>,
    pub(crate) is_destructor: bool,
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
pub(super) enum ResourceAction<F> {
    New,
    Call(F),
    Static(F),
    Drop(Option<F>),
}

#[derive(Clone, Copy)]
pub(super) enum SourceTarget {
    Function(Span),
    Resource {
        owner: usize,
        action: ResourceAction<Span>,
    },
}

pub(super) struct ResourceClass<T> {
    pub(super) class: T,
    pub(super) resource: wit_parser::TypeId,
    pub(super) create: String,
    pub(super) representation: String,
    pub(super) source_ty: HirType,
}

pub(super) struct ClassCall {
    pub(super) owner: usize,
    pub(super) action: ResourceAction<FuncId>,
}

pub(super) struct Roots {
    namespaces: Vec<Span>,
    pub(super) witnesses: Vec<Witness<SourceTarget>>,
    classes: Vec<ResourceClass<Span>>,
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

    fn member_declaration(
        &self,
        object: &'a ast::ObjectLit,
        name: &str,
    ) -> Option<Declaration<'a>> {
        object.props.iter().rev().find_map(|p| {
            let ast::PropOrSpread::Prop(p) = p else {
                return None;
            };
            self.declaration(match &**p {
                ast::Prop::Shorthand(id) if id.sym == name => self.0.get(&id.to_id())?.1,
                ast::Prop::KeyValue(kv) if key(&kv.key) == Some(name) => {
                    Declaration::Value(&kv.value)
                }
                ast::Prop::Method(m) if key(&m.key) == Some(name) => {
                    if m.function.is_async || m.function.is_generator {
                        Declaration::UnsupportedMethod(m.span())
                    } else {
                        Declaration::Function(m.span(), &m.function)
                    }
                }
                _ => return None,
            })
        })
    }
}

pub(super) fn resolve<'a>(
    module: &'a ast::Module,
    exports: &BTreeMap<String, ast::Ident>,
    world: Option<(&Resolve, WorldId)>,
    locations: &Locations<'_>,
    unresolved: swc_common::SyntaxContext,
) -> Result<Roots> {
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
    let exported = |name: &str| {
        let id = exports.get(name)?;
        scope
            .0
            .values()
            .find(|(i, _)| i.span == id.span)
            .and_then(|(_, d)| scope.declaration(*d))
    };
    let mut roots = Roots {
        namespaces: Vec::new(),
        witnesses: Vec::new(),
        classes: Vec::new(),
    };
    let Some((r, w)) = world else {
        for name in exports.keys() {
            if let Some(span) = exported(name).and_then(Declaration::function_span) {
                roots.witnesses.push(Witness {
                    name: name.clone(),
                    signature: None,
                    target: Some(SourceTarget::Function(span)),
                    is_destructor: false,
                });
            }
        }
        roots.witnesses.sort_by_key(|w| match w.target {
            Some(SourceTarget::Function(span)) => Some(span.lo),
            _ => None,
        });
        return Ok(roots);
    };
    let world = &r.worlds[w];
    let namespace = |k: &WorldKey, id| {
        let n = interface_name(k, r, id)?;
        match exported(&to_camel_case(n))? {
            Declaration::Value(ast::Expr::Object(o)) => Some(o),
            _ => None,
        }
    };
    use swc_ecma_visit::{Visit, VisitWith};
    struct Uses<'a>(&'a Scope<'a>, Vec<Span>);
    impl Visit for Uses<'_> {
        fn visit_ident(&mut self, id: &ast::Ident) {
            if let Some((binding, _)) = self.0.0.get(&id.to_id())
                && binding.span != id.span
            {
                self.1.retain(|&s| s != binding.span);
            }
        }
    }
    let mut disposals = Vec::new();
    let mut class_asts: Vec<&'a ast::Class> = Vec::new();
    for (k, item) in &world.exports {
        let wit_parser::WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let Some(obj) = namespace(k, *id) else {
            continue;
        };
        if let Some(binding) =
            interface_name(k, r, *id).and_then(|n| exports.get(&to_camel_case(n)))
        {
            roots.namespaces.push(binding.span);
        }
        for (name, resource) in &r.interfaces[*id].types {
            if !matches!(r.types[*resource].kind, wit_parser::TypeDefKind::Resource) {
                continue;
            }
            let Some(Declaration::Class(class)) =
                scope.member_declaration(obj, &to_pascal_case(name))
            else {
                continue;
            };
            let module = r.name_world_key(k);
            let source_ty = crate::wit::source_type(r, *resource)
                .ok_or_else(|| Diagnostic::internal("unnamed exported resource"))?;
            let owner = roots.classes.len();
            class_asts.push(class);
            roots.classes.push(ResourceClass {
                resource: *resource,
                class: class.span,
                source_ty,
                create: format!("{module}#new{}", to_pascal_case(name)),
                representation: format!("{module}#{}Rep", to_camel_case(name)),
            });
            let member = class.body.iter().find_map(|m| {
                let ast::ClassMember::Method(m) = m else {
                    return None;
                };
                let ast::PropName::Computed(k) = &m.key else {
                    return None;
                };
                let ast::Expr::Member(e) = k.expr.unwrap_parens() else {
                    return None;
                };
                (matches!(&*e.obj,ast::Expr::Ident(id) if id.sym=="Symbol" && id.ctxt==unresolved)
                    && matches!(&e.prop,ast::MemberProp::Ident(i) if i.sym=="dispose"))
                .then_some(m.span)
            });
            disposals.push(Witness {
                name: format!("{module}#[dtor]{name}"),
                signature: Some(Signature {
                    annotations: Vec::new(),
                    params: vec![HirType::Number],
                    result: HirType::Void,
                }),
                target: Some(SourceTarget::Resource {
                    owner,
                    action: ResourceAction::Drop(member),
                }),
                is_destructor: true,
            });
        }
    }
    let mut uses = Uses(&scope, std::mem::take(&mut roots.namespaces));
    module.visit_with(&mut uses);
    roots.namespaces = uses.1;
    let mut legacy_names = BTreeMap::new();
    for entry in perry_contract::world_functions(r, world.exports.iter()) {
        let f = entry.function;
        let name = entry.implementation_name(r, world);
        let requirement = if entry.interface.is_some() {
            perry_contract::export_names::core_export_name(r, entry.key, f)
        } else {
            f.name.clone()
        };
        let object = entry.interface.and_then(|id| namespace(entry.key, id));
        let mut target = None;
        let mut annotations = Vec::new();
        if let Some(resource) = f.kind.resource()
            && let Some(owner) = roots.classes.iter().position(|c| c.resource == resource)
        {
            let class = class_asts[owner];
            let find_method = |is_static: bool| {
                let method_name = to_camel_case(f.item_name());
                class.body.iter().find_map(|m| match m {
                    ast::ClassMember::Method(m)
                        if m.is_static == is_static
                            && key(&m.key) == Some(method_name.as_str()) =>
                    {
                        Some(m.span)
                    }
                    _ => None,
                })
            };
            let action = match f.kind {
                wit_parser::FunctionKind::Constructor(_) => Some(ResourceAction::New),
                wit_parser::FunctionKind::Static(_) | wit_parser::FunctionKind::AsyncStatic(_) => {
                    find_method(true).map(ResourceAction::Static)
                }
                _ => find_method(false).map(ResourceAction::Call),
            };
            if let Some(action) = action {
                target = Some(SourceTarget::Resource { owner, action });
            }
        } else if let Some(decl) =
            object.and_then(|o| scope.member_declaration(o, &to_camel_case(&f.name)))
        {
            match decl {
                decl if decl.function_span().is_some() => {target = decl.function_span().map(SourceTarget::Function); annotations=decl.annotations();},
                Declaration::UnsupportedMethod(span) => return Err(locations.annotate(
                    Diagnostic::unsupported(format!("async and generator method syntax is unsupported for WIT export {requirement}"))
                        .with_help("Export a reference to a function declaration in the interface object."), span)),
                _ => {}
            }
        }
        if target.is_none()
            && let Some(decl) = exported(&name)
            && let Some(span) = decl.function_span()
        {
            if let Some(previous) = legacy_names.insert(name.clone(), requirement.clone()) {
                return Err(locations.annotate(diag!(codes::CONTRACT_MISMATCH,"WIT exports '{previous}' and '{requirement}' both require TypeScript implementation '{name}'"),span));
            }
            target = Some(SourceTarget::Function(span));
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
        let ty = |mut t| {
            while let wit_parser::Type::Id(id) = t
                && let wit_parser::TypeDefKind::Type(inner) = r.types[id].kind
            {
                t = inner;
            }
            use wit_parser::Type::*;
            Ok(match t {
                U8 | S8 | U16 | S16 | U32 | S32 => HirType::Number,
                U64 | S64 => HirType::BigInt,
                _ => crate::wit::source_type_of(r, Input::Reference(t)).ok_or_else(|| {
                    Diagnostic::unsupported(format!(
                        "WIT export {requirement} uses an unsupported type"
                    ))
                })?,
            })
        };
        roots.witnesses.push(Witness {
            name,
            target,
            signature: Some(Signature {
                annotations,
                params: f.params.iter().map(|p| ty(p.ty)).collect::<Result<_>>()?,
                result: f.result.map(ty).transpose()?.unwrap_or(HirType::Void),
            }),
            is_destructor: false,
        });
    }
    roots.witnesses.extend(disposals);
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

impl Roots {
    pub(super) fn in_hir(self, module: &mut perry_hir::ir::Module) -> Result<Vec<Witness<FuncId>>> {
        use perry_hir::ir::{Expr, Stmt};
        module.init.retain(|stmt| {
            let Stmt::Let {
                id,
                mutable: false,
                init: Some(init),
                ..
            } = stmt
            else {
                return true;
            };
            let refs = match init {
                Expr::Object(fields) => fields
                    .iter()
                    .all(|(_, e)| matches!(e, Expr::FuncRef(_) | Expr::ClassRef(_))),
                Expr::New {
                    class_name, args, ..
                } if super::is_object_literal(class_name) => args
                    .iter()
                    .all(|e| matches!(e, Expr::FuncRef(_) | Expr::ClassRef(_))),
                _ => false,
            };
            !(refs
                && module
                    .local_source_spans
                    .get(id)
                    .is_some_and(|s| self.namespaces.iter().any(|span| span.lo.0 == s.start)))
        });
        let classes = self
            .classes
            .into_iter()
            .map(|c| {
                let range = c.class.lo.0.saturating_sub(1) as usize
                    ..c.class.hi.0.saturating_sub(1) as usize;
                let id = module
                    .class_source_text
                    .iter()
                    .find_map(|(id, s)| (s.source_range() == range).then_some(*id))
                    .ok_or_else(|| Diagnostic::internal("resource class has no HIR declaration"))?;
                let class_idx = module
                    .classes
                    .iter()
                    .position(|cls| cls.id == id)
                    .ok_or_else(|| Diagnostic::internal("lost resource class"))?;
                Ok(ResourceClass {
                    class: class_idx,
                    resource: c.resource,
                    create: c.create,
                    representation: c.representation,
                    source_ty: c.source_ty,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let mut calls = Vec::with_capacity(self.witnesses.len());
        let mut witnesses = Vec::with_capacity(self.witnesses.len());
        for w in self.witnesses {
            let id = |span: Span| {
                module
                    .closure_source_text
                    .iter()
                    .find_map(|(id, meta)| {
                        (meta.source_span == Some((span.lo.0, span.hi.0))).then_some(*id)
                    })
                    .ok_or_else(|| {
                        Diagnostic::internal(format!("no HIR declaration for export '{}'", w.name))
                    })
            };
            let (target, call) = match w.target {
                None => (None, None),
                Some(SourceTarget::Function(span)) => (Some(id(span)?), None),
                Some(SourceTarget::Resource { owner, action }) => {
                    let action = match action {
                        ResourceAction::New => ResourceAction::New,
                        ResourceAction::Call(span) => ResourceAction::Call(id(span)?),
                        ResourceAction::Static(span) => ResourceAction::Static(id(span)?),
                        ResourceAction::Drop(span) => {
                            ResourceAction::Drop(span.map(id).transpose()?)
                        }
                    };
                    (None, Some(ClassCall { owner, action }))
                }
            };
            calls.push(call);
            witnesses.push(Witness {
                name: w.name,
                signature: w.signature,
                target,
                is_destructor: w.is_destructor,
            });
        }
        if !classes.is_empty() {
            let (local, function) = perry_hir::lexical_frames::max_ids(module);
            super::resource_classes::bind(
                module,
                &classes,
                &calls,
                &mut witnesses,
                local,
                function,
            )?;
        }
        Ok(witnesses)
    }
}
