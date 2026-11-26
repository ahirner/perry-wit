//! Source declarations and executable bodies have separate lifetimes during specialization.
use crate::engine::{BodyOwner, types::LocalFlags};
use perry_hir::{
    ir::{Class, Expr, Function, Import, Interface, Module, Param, Stmt, TypeAlias},
    types::{FuncId, LocalId},
};
use std::collections::HashMap;

#[derive(Clone, Copy)]
pub(crate) enum Arguments<'s> {
    Plain(&'s [Expr]),
    Constructor(&'s [Expr]),
    Spread(&'s [perry_hir::CallArg]),
    OneSpread(&'s Expr),
}

impl<'s> Iterator for Arguments<'s> {
    type Item = (&'s Expr, bool);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Plain(args) | Self::Constructor(args) => Some((args.split_off_first()?, false)),
            Self::Spread(args) => Some(match args.split_off_first()? {
                perry_hir::CallArg::Expr(expr) => (expr, false),
                perry_hir::CallArg::Spread(expr) => (expr, true),
            }),
            Self::OneSpread(expr) => {
                let arg = (*expr, true);
                *self = Self::Plain(&[]);
                Some(arg)
            }
        }
    }
}

pub(crate) struct ModuleDeclarations {
    pub(crate) name: String,
    pub(crate) imports: Vec<Import>,
    pub(crate) interfaces: Vec<Interface>,
    pub(crate) type_aliases: Vec<TypeAlias>,
    pub(crate) locals: HashMap<LocalId, LocalDeclaration>,
    pub(crate) closure_signatures: HashMap<FuncId, TypeAlias>,
    pub(crate) has_top_level_await: bool,
}

impl ModuleDeclarations {
    pub(crate) fn for_of<'a>(&self, init: Option<&'a Stmt>) -> Option<(LocalId, &'a Expr)> {
        let Stmt::Let {
            id,
            init: Some(source),
            ..
        } = init?
        else {
            return None;
        };
        self.locals
            .get(id)?
            .flags
            .contains(LocalFlags::FOR_OF)
            .then_some((*id, source))
    }
}

pub(crate) struct LocalDeclaration {
    pub(crate) span: swc_common::Span,
    pub(crate) flags: LocalFlags,
}

pub(crate) struct SourceModule<'s> {
    pub(crate) member_prefix: String,
    pub(crate) constructors: super::Constructors,
    pub(crate) locations: crate::hir_compat::Locations<'s>,
    pub(crate) exports: crate::hir_compat::Exports<'s>,
    pub(crate) sparse_arrays: bool,
    pub(crate) declarations: ModuleDeclarations,
    pub(crate) bodies: Bodies,
    pub(crate) closure_display_names: HashMap<FuncId, String>,
}

impl<'s> SourceModule<'s> {
    pub(crate) fn bind(
        mut module: Module,
        mut analysis: super::Analysis,
        constructors: super::Constructors,
        locations: crate::hir_compat::Locations<'s>,
        exports: crate::hir_compat::Exports<'s>,
    ) -> Self {
        let locals = module
            .local_source_spans
            .into_iter()
            .map(|(id, span)| {
                let syntax = analysis
                    .bindings
                    .get(&span.start)
                    .copied()
                    .unwrap_or_default();
                let span = swc_common::Span::new(
                    swc_common::BytePos(span.start),
                    swc_common::BytePos(span.end),
                );
                let mut flags = syntax.flags;
                if syntax.iteration_end == Some(span.hi.0) {
                    flags.insert(LocalFlags::FOR_OF);
                    flags.insert(LocalFlags::INFERRED);
                } else if syntax.inferred_end == Some(span.hi.0) {
                    flags.insert(LocalFlags::INFERRED);
                }
                (id, LocalDeclaration { span, flags })
            })
            .collect();
        let mut closure_signatures = HashMap::new();
        if !analysis.signature_prefix.is_empty() {
            let mut aliases: HashMap<_, _> = module
                .type_aliases
                .extract_if(.., |a| a.name.starts_with(&analysis.signature_prefix))
                .map(|a| (a.name.clone(), a))
                .collect();
            for (id, source) in module.closure_source_text {
                if let Some((lo, _)) = source.source_span
                    && let Some(alias) =
                        aliases.remove(&format!("{}{lo}", analysis.signature_prefix))
                {
                    closure_signatures.insert(id, alias);
                }
            }
        }
        Self {
            locations,
            constructors,
            exports,
            member_prefix: analysis.names.prefix("member"),
            sparse_arrays: analysis.sparse_arrays,
            declarations: ModuleDeclarations {
                name: module.name,
                imports: module.imports,
                interfaces: module.interfaces,
                type_aliases: module.type_aliases,
                locals,
                closure_signatures,
                has_top_level_await: module.has_top_level_await,
            },
            bodies: Bodies {
                init: module.init,
                functions: module.functions,
                classes: module.classes,
            },
            closure_display_names: module.closure_display_names,
        }
    }
}

pub(crate) struct Bodies {
    pub(crate) init: Vec<Stmt>,
    pub(crate) functions: Vec<Function>,
    pub(crate) classes: Vec<Class>,
}

/// Perry also emits default parameter assignments as a prefix of the function body.
/// Parameter binding owns their evaluation. Match at most one generated assignment per default.
pub(crate) fn authored_body<'a>(params: &[Param], mut body: &'a [Stmt]) -> &'a [Stmt] {
    for param in params.iter().filter(|p| !p.is_rest && p.default.is_some()) {
        let Some((
            Stmt::If {
                condition:
                    Expr::Compare {
                        op: perry_hir::ir::CompareOp::Eq,
                        left,
                        right,
                    },
                then_branch,
                else_branch: None,
            },
            rest,
        )) = body.split_first()
        else {
            break;
        };
        if !matches!(left.as_ref(), Expr::LocalGet(id) if *id == param.id)
            || !matches!(right.as_ref(), Expr::Undefined)
            || !matches!(then_branch.as_slice(), [Stmt::Expr(Expr::LocalSet(id, _))] if *id == param.id)
        {
            break;
        }
        body = rest;
    }
    body
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CallerKind {
    Initializer,
    Export,
    Function,
    Class,
}

pub(crate) struct InstanceFunction<'s> {
    pub(crate) template: crate::engine::types::TemplateId,
    pub(crate) id: FuncId,
    pub(crate) name: std::borrow::Cow<'s, str>,
    pub(crate) params: Vec<(&'s Param, Option<crate::engine::types::Ty>)>,
    pub(crate) inline: Option<crate::engine::solve::Invocation>,
    pub(crate) environment: Option<crate::engine::types::FuncIdentity>,
    pub(crate) receiver: Option<crate::engine::types::Ty>,
    pub(crate) result: crate::engine::InstanceResult,
    pub(crate) is_exported: bool,
}

pub(crate) struct StaticField<'s> {
    pub(crate) id: LocalId,
    pub(crate) field: &'s perry_hir::ir::ClassField,
    pub(crate) ty: Option<crate::engine::types::Ty>,
}

pub(crate) struct InstanceClass<'s> {
    pub(crate) ty: crate::engine::types::Ty,
    pub(crate) declaration: &'s Class,
    pub(crate) name: String,
    pub(crate) fields: Vec<(
        &'s perry_hir::ir::ClassField,
        Option<crate::engine::types::Ty>,
    )>,
    pub(crate) initializer: Option<FuncId>,
    pub(crate) constructor: Option<InstanceFunction<'s>>,
    pub(crate) direct_constructor: bool,
}

impl InstanceClass<'_> {
    pub(crate) fn factory(&self) -> perry_wit_diagnostics::Result<FuncId> {
        self.constructor.as_ref().map(|f| f.id).ok_or_else(|| {
            perry_wit_diagnostics::internal!("missing constructor for {}", self.name)
        })
    }
}

pub(crate) struct InstanceBodies<'s> {
    pub(crate) static_fields: std::collections::BTreeMap<(&'s str, &'s str), StaticField<'s>>,
    pub(crate) exports: Vec<Option<FuncId>>,
    pub(crate) native: Vec<(FuncId, crate::engine::NativeFunction)>,
    pub(crate) init: &'s [Stmt],
    pub(crate) functions: Vec<InstanceFunction<'s>>,
    pub(crate) classes:
        crate::engine::types::IdxVec<crate::engine::types::ClassInstId, InstanceClass<'s>>,
}

impl crate::engine::InstancePlans {
    pub(crate) fn reference(&self, owner: BodyOwner, expr: &Expr) -> Option<FuncId> {
        self.reference_at(owner, expr, self.site(owner, expr).resolution)
    }

    pub(crate) fn reference_at(
        &self,
        owner: BodyOwner,
        expr: &Expr,
        resolution: Option<crate::engine::solve::SiteResolution>,
    ) -> Option<FuncId> {
        if let Some(crate::engine::solve::SiteResolution::Callback { target }) = resolution {
            return Some(self.function(target));
        }
        match *expr {
            Expr::LocalGet(id) => self.erased_parameter_declaration(owner, id),
            Expr::FuncRef(id) => Some(id),
            _ => None,
        }
    }
    pub(crate) fn call_target(&self, owner: BodyOwner, expr: &Expr) -> Option<FuncId> {
        if let Some(crate::engine::solve::SiteResolution::Call { target, .. }) =
            self.site(owner, expr).resolution
        {
            return self.emitted_callee(target)?.ok();
        }
        match expr {
            Expr::Call { callee, .. } | Expr::CallSpread { callee, .. } => {
                self.reference(owner, callee)
            }
            _ => None,
        }
    }
}
pub(crate) fn is_pure_func_expr(expr: &Expr) -> bool {
    match expr {
        Expr::FuncRef(_) | Expr::Closure { .. } | Expr::LocalGet(_) | Expr::This => true,
        Expr::PropertyGet { object, .. } => is_pure_func_expr(object),
        _ => false,
    }
}
