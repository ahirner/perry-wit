//! Source declarations and executable bodies have separate lifetimes during specialization.
use perry_hir::{
    ir::{
        Class, Expr, Function, Import, Interface, LocalSourceSpan, Module, Param, Stmt, TypeAlias,
    },
    types::{FuncId, LocalId},
    walker::{stmt_any_expr, walk_expr_children},
};
use std::collections::{HashMap, HashSet};

pub(crate) struct ModuleDeclarations {
    pub(crate) name: String,
    pub(crate) imports: Vec<Import>,
    pub(crate) interfaces: Vec<Interface>,
    pub(crate) type_aliases: Vec<TypeAlias>,
    pub(crate) local_source_spans: HashMap<LocalId, LocalSourceSpan>,
    pub(crate) has_top_level_await: bool,
}

pub(crate) struct SourceModule {
    pub(crate) declarations: ModuleDeclarations,
    pub(crate) bodies: Bodies,
    pub(crate) closure_display_names: HashMap<FuncId, String>,
}

impl From<Module> for SourceModule {
    fn from(module: Module) -> Self {
        Self {
            declarations: ModuleDeclarations {
                name: module.name,
                imports: module.imports,
                interfaces: module.interfaces,
                type_aliases: module.type_aliases,
                local_source_spans: module.local_source_spans,
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

pub(crate) struct BodyIndex<'a> {
    pub(crate) closures: HashMap<FuncId, &'a Expr>,
    pub(crate) globals: HashSet<LocalId>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum CallerKind {
    Initializer,
    Export,
    Function,
    Class,
}

impl Bodies {
    pub(crate) fn index<'a>(
        &'a self,
        locations: &mut crate::hir_compat::Locations<'_>,
        plans: &crate::engine::InstancePlans,
    ) -> BodyIndex<'a> {
        let module_locals: HashSet<_> = self
            .init
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::Let { id, .. } => Some(*id),
                _ => None,
            })
            .collect();
        let names: HashMap<_, _> = self
            .functions
            .iter()
            .filter(|_| locations.has_imports())
            .map(|f| (f.id, f.name.as_str()))
            .collect();
        let mut index = BodyIndex {
            closures: HashMap::new(),
            globals: HashSet::new(),
        };
        let mut scan = |expr: &'a Expr, kind, caller: Option<&str>| match expr {
            Expr::Closure { func_id, .. } => {
                index.closures.entry(*func_id).or_insert(expr);
            }
            Expr::Call {
                callee,
                byte_offset,
                ..
            } if let Expr::FuncRef(id) = &**callee
                && let Some(&callee) = names.get(id) =>
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
                if kind != CallerKind::Initializer && module_locals.contains(id) =>
            {
                index.globals.insert(*id);
            }
            _ => {}
        };
        for function in &self.functions {
            let kind = if function.is_exported {
                CallerKind::Export
            } else {
                CallerKind::Function
            };
            plans.walk_function(function, &mut |expr| scan(expr, kind, Some(&function.name)));
        }
        for class in &self.classes {
            let mut scan_class = |expr| scan(expr, CallerKind::Class, None);
            for field in class.fields.iter().chain(&class.static_fields) {
                if let Some(init) = &field.init {
                    plans.walk_expr(init, &mut scan_class);
                }
            }
            for function in class_functions(class) {
                plans.walk_function(function, &mut scan_class);
            }
        }
        plans.walk_body(&self.init, &mut |expr| {
            scan(expr, CallerKind::Initializer, None)
        });
        index
    }
}

fn class_functions(class: &Class) -> impl Iterator<Item = &Function> {
    class
        .constructor
        .iter()
        .chain(&class.methods)
        .chain(&class.static_methods)
        .chain(class.getters.iter().map(|(_, method)| method))
        .chain(class.setters.iter().map(|(_, method)| method))
        .chain(class.computed_members.iter().map(|member| &member.function))
}

impl crate::engine::InstancePlans {
    pub(crate) fn function_body<'a>(&self, f: &'a Function) -> &'a [Stmt] {
        if self.signatures.contains_key(&f.id) {
            &f.body
        } else {
            authored_body(&f.params, &f.body)
        }
    }

    fn walk_function<'a>(&self, function: &'a Function, visit: &mut impl FnMut(&'a Expr)) {
        for param in &function.params {
            if let Some(default) = &param.default {
                self.walk_expr(default, visit);
            }
        }
        self.walk_body(self.function_body(function), visit);
    }

    fn walk_body<'a>(&self, body: &'a [Stmt], visit: &mut impl FnMut(&'a Expr)) {
        for stmt in body {
            stmt_any_expr(stmt, &mut |expr| {
                self.walk_expr(expr, visit);
                false
            });
        }
    }

    fn walk_expr<'a>(&self, expr: &'a Expr, visit: &mut impl FnMut(&'a Expr)) {
        if let Expr::Closure { params, body, .. } = expr {
            self.walk_body(authored_body(params, body), visit);
        }
        walk_expr_children(expr, &mut |child| self.walk_expr(child, visit));
        visit(expr);
    }
}
