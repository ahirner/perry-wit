//! Source declarations and executable bodies have separate lifetimes during specialization.
use crate::engine::{BodyOwner, types::LocalFlags};
use perry_hir::{
    ir::{Class, Expr, Function, Import, Interface, Module, Param, Stmt, TypeAlias},
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use std::collections::{HashMap, HashSet};

pub(crate) struct ModuleDeclarations {
    pub(crate) name: String,
    pub(crate) imports: Vec<Import>,
    pub(crate) interfaces: Vec<Interface>,
    pub(crate) type_aliases: Vec<TypeAlias>,
    pub(crate) locals: HashMap<LocalId, LocalDeclaration>,
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
    pub(crate) constructors: super::Constructors,
    pub(crate) locations: crate::hir_compat::Locations<'s>,
    pub(crate) witnesses: Vec<crate::hir_compat::Witness<'s, FuncId>>,
    pub(crate) sparse_arrays: bool,
    pub(crate) declarations: ModuleDeclarations,
    pub(crate) bodies: Bodies,
    pub(crate) closure_display_names: HashMap<FuncId, String>,
}

impl<'s> SourceModule<'s> {
    pub(crate) fn bind(
        module: Module,
        analysis: super::Analysis,
        constructors: super::Constructors,
        locations: crate::hir_compat::Locations<'s>,
        witnesses: Vec<crate::hir_compat::Witness<'s, FuncId>>,
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
        Self {
            locations,
            constructors,
            witnesses,
            sparse_arrays: analysis.sparse_arrays,
            declarations: ModuleDeclarations {
                name: module.name,
                imports: module.imports,
                interfaces: module.interfaces,
                type_aliases: module.type_aliases,
                locals,
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
    pub(crate) signature: Option<crate::engine::InstanceSignature>,
    pub(crate) id: FuncId,
    pub(crate) name: std::borrow::Cow<'s, str>,
    pub(crate) params: Vec<(&'s Param, Option<crate::engine::types::Ty>)>,
    pub(crate) return_type: &'s perry_hir::types::Type,
    pub(crate) body: &'s [Stmt],
    pub(crate) captures: std::borrow::Cow<'s, [LocalId]>,
    pub(crate) is_async: bool,
    pub(crate) is_exported: bool,
    pub(crate) is_generator: bool,
}

impl<'s> From<&'s Function> for InstanceFunction<'s> {
    fn from(function: &'s Function) -> Self {
        Self {
            signature: None,
            id: function.id,
            name: function.name.as_str().into(),
            params: function.params.iter().map(|param| (param, None)).collect(),
            return_type: &function.return_type,
            body: authored_body(&function.params, &function.body),
            captures: function.captures.as_slice().into(),
            is_async: function.is_async,
            is_exported: function.is_exported,
            is_generator: function.is_generator,
        }
    }
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
    pub(crate) static_fields: Vec<StaticField<'s>>,
    pub(crate) initializer: Option<FuncId>,
    pub(crate) constructor: Option<InstanceFunction<'s>>,
    pub(crate) methods: Vec<InstanceFunction<'s>>,
    pub(crate) static_methods: Vec<InstanceFunction<'s>>,
    pub(crate) getters: Vec<(&'s str, InstanceFunction<'s>)>,
    pub(crate) setters: Vec<(&'s str, InstanceFunction<'s>)>,
}

pub(crate) struct InstanceBodies<'s> {
    pub(crate) native: Vec<(FuncId, crate::engine::NativeFunction)>,
    pub(crate) init: &'s [Stmt],
    pub(crate) functions: Vec<InstanceFunction<'s>>,
    pub(crate) classes:
        crate::engine::types::IdxVec<crate::engine::types::ClassInstId, InstanceClass<'s>>,
}

impl InstanceBodies<'_> {
    pub(crate) fn index<'a>(
        &'a self,
        locations: &mut crate::hir_compat::Locations<'_>,
        plans: &'a crate::engine::InstancePlans,
    ) -> HashSet<LocalId> {
        let names: HashMap<_, _> = self
            .functions
            .iter()
            .filter(|_| locations.has_imports())
            .map(|f| (f.id, f.name.as_ref()))
            .collect();
        let mut globals = HashSet::new();
        let mut scan = |owner, expr: &'a Expr, kind, caller: Option<&str>| match expr {
            Expr::Call { byte_offset, .. }
                if let Some(id) = plans.call_target(owner, expr)
                    && let Some(&callee) = names.get(&id) =>
            {
                locations.record_cross_file_call(callee, *byte_offset, caller, kind);
            }
            Expr::LocalGet(id)
            | Expr::LocalSet(id, _)
            | Expr::Update { id, .. }
            | Expr::ArrayPush { array_id: id, .. }
            | Expr::ArrayPushSpread { array_id: id, .. }
            | Expr::ArrayUnshift { array_id: id, .. }
            | Expr::ArraySplice { array_id: id, .. }
            | Expr::ArrayCopyWithin { array_id: id, .. }
            | Expr::ArrayPop(id)
            | Expr::ArrayShift(id)
            | Expr::SetAdd { set_id: id, .. }
                if kind != CallerKind::Initializer && plans.module_binding(*id) =>
            {
                globals.insert(*id);
            }
            _ => {}
        };
        for function in &self.functions {
            let kind = if function.is_exported {
                CallerKind::Export
            } else {
                CallerKind::Function
            };
            self.walk_function(function, &mut |owner, expr| {
                scan(owner, expr, kind, Some(&function.name))
            });
        }
        for class in self.classes.iter() {
            let mut scan_class = |owner, expr| scan(owner, expr, CallerKind::Class, None);
            for &(field, _) in &class.fields {
                if let Some(init) = &field.init {
                    self.walk_expr(
                        class
                            .initializer
                            .map_or(BodyOwner::Initializer, BodyOwner::Function),
                        init,
                        &mut scan_class,
                    );
                }
            }
            for field in &class.declaration.static_fields {
                if let Some(init) = &field.init {
                    self.walk_expr(BodyOwner::Initializer, init, &mut scan_class);
                }
            }
            for function in class
                .constructor
                .iter()
                .chain(&class.methods)
                .chain(&class.static_methods)
                .chain(class.getters.iter().chain(&class.setters).map(|(_, f)| f))
            {
                self.walk_function(function, &mut scan_class);
            }
            for member in &class.declaration.computed_members {
                let f = &member.function;
                self.walk_body(
                    BodyOwner::Function(f.id),
                    authored_body(&f.params, &f.body),
                    &mut scan_class,
                );
                for default in f.params.iter().filter_map(|p| p.default.as_ref()) {
                    self.walk_expr(BodyOwner::Function(f.id), default, &mut scan_class);
                }
            }
        }
        self.walk_body(BodyOwner::Initializer, self.init, &mut |owner, expr| {
            scan(owner, expr, CallerKind::Initializer, None)
        });
        globals
    }
}

impl crate::engine::InstancePlans {
    pub(crate) fn reference(&self, owner: BodyOwner, expr: &Expr) -> Option<FuncId> {
        if let Some(crate::engine::solve::SiteResolution::Callback { target }) =
            self.site(owner, expr).resolution
        {
            return Some(self.function(target));
        }
        match *expr {
            Expr::LocalGet(id) => self.parameter_function(owner, id),
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
impl InstanceBodies<'_> {
    fn walk_function<'a>(
        &'a self,
        function: &'a InstanceFunction<'a>,
        visit: &mut impl FnMut(BodyOwner, &'a Expr),
    ) {
        let owner = BodyOwner::Function(function.id);
        for &(param, _) in &function.params {
            if let Some(default) = &param.default {
                self.walk_expr(owner, default, visit);
            }
        }
        self.walk_body(owner, function.body, visit);
    }
    fn walk_body<'a>(
        &'a self,
        owner: BodyOwner,
        body: &'a [Stmt],
        visit: &mut impl FnMut(BodyOwner, &'a Expr),
    ) {
        for stmt in body {
            stmt_any_expr(stmt, &mut |expr| {
                self.walk_expr(owner, expr, visit);
                false
            });
        }
    }
    fn walk_expr<'a>(
        &'a self,
        owner: BodyOwner,
        expr: &'a Expr,
        visit: &mut impl FnMut(BodyOwner, &'a Expr),
    ) {
        if let Expr::Closure {
            func_id,
            params,
            body,
            ..
        } = expr
        {
            self.walk_body(
                BodyOwner::Function(*func_id),
                authored_body(params, body),
                visit,
            );
        }
        walk_expr_children(expr, &mut |child| self.walk_expr(owner, child, visit));
        visit(owner, expr);
    }
}

pub(crate) fn is_pure_func_expr(expr: &Expr) -> bool {
    match expr {
        Expr::FuncRef(_) | Expr::Closure { .. } | Expr::LocalGet(_) | Expr::This => true,
        Expr::PropertyGet { object, .. } => is_pure_func_expr(object),
        _ => false,
    }
}
