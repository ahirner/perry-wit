//! Resolve single-identity callable bindings before specializing their calls.
use super::*;
use perry_hir::ir::CompareOp;
use std::rc::Rc;

#[derive(Default)]
pub(super) struct Callables {
    pub lifted_captures: HashSet<LocalId>,
    pub invoked: HashSet<LocalId>,
    pub templates: HashMap<FuncId, Rc<Function>>,
    pub aliases: HashMap<LocalId, FuncId>,
    pub captures: HashMap<LocalId, HirType>,
    pub origins: HashMap<FuncId, FuncId>,
    pub signatures: HashMap<FuncId, perry_hir::types::FunctionType>,
}

pub(super) fn signature(function: &Function) -> perry_hir::types::FunctionType {
    perry_hir::types::FunctionType {
        params: function
            .params
            .iter()
            .map(|p| (p.name.clone(), p.ty.clone(), p.default.is_some()))
            .collect(),
        return_type: Box::new(function.return_type.clone()),
        is_async: function.is_async,
        is_generator: function.is_generator,
    }
}

pub(super) fn implicit_parameters(function: &mut Function, implicit: impl Fn(usize) -> bool) {
    for (index, param) in function.params.iter_mut().enumerate() {
        if implicit(index) && matches!(param.ty, HirType::Any | HirType::Unknown) {
            let name = format!("@{}", param.name);
            param.ty = HirType::TypeVar(name.clone());
            function.type_params.push(perry_hir::types::TypeParam {
                name,
                constraint: None,
                default: None,
            });
        }
    }
}

impl Callables {
    pub(super) fn lift(&mut self, expr: &mut Expr, name: Option<&str>) {
        let Expr::Closure {
            func_id,
            captures_this: false,
            captures_new_target: false,
            ..
        } = expr
        else {
            return;
        };
        let id = *func_id;
        let mut references = Vec::new();
        perry_hir::analysis::collect_local_refs_expr(expr, &mut references, &mut HashSet::new());
        let lifted = std::mem::replace(expr, Expr::FuncRef(id));
        let Expr::Closure {
            params,
            return_type,
            body,
            captures,
            is_async,
            is_generator,
            is_strict,
            construct_only,
            ..
        } = lifted
        else {
            // Matched as a closure above.
            *expr = lifted;
            return;
        };
        self.lifted_captures.extend(references);
        self.lifted_captures.extend(captures.iter().copied());
        let mut function = Function {
            id,
            name: name
                .map(str::to_owned)
                .unwrap_or_else(|| format!("callback_{id}")),
            type_params: Vec::new(),
            params,
            return_type,
            body,
            captures,
            is_async,
            is_generator,
            is_strict,
            construct_only,
            is_exported: false,
            decorators: Vec::new(),
            was_plain_async: false,
            was_unrolled: false,
        };
        implicit_parameters(&mut function, |_| true);
        self.signatures.insert(id, signature(&function));
        self.templates.insert(id, Rc::new(function));
    }

    pub(super) fn prepare(&mut self, body: &mut [Stmt], env: &FunctionEnv<'_>) -> Result<()> {
        struct Resolve<'a, 'e> {
            facts: &'a mut Callables,
            env: &'a FunctionEnv<'e>,
        }
        impl Resolve<'_, '_> {
            fn origin(&self, id: FuncId) -> FuncId {
                self.facts.origins.get(&id).copied().unwrap_or(id)
            }
        }
        impl Visitor for Resolve<'_, '_> {
            type Error = Diagnostic;
            fn node(&mut self, node: Node<'_>) -> Result<()> {
                let Node::Expression(expr) = node else {
                    return Ok(());
                };
                match expr {
                    Expr::LocalGet(id) => {
                        if let Some(target) = self.facts.aliases.get(id) {
                            *expr = Expr::FuncRef(*target);
                        }
                    }
                    Expr::LocalSet(id, _) | Expr::Update { id, .. }
                        if self.facts.aliases.contains_key(id) =>
                    {
                        return Err(Diagnostic::unsupported(
                            "a reassigned function binding requires a runtime function representation",
                        ));
                    }
                    Expr::Compare { op, left, right } => {
                        if let (Expr::FuncRef(a), Expr::FuncRef(b)) =
                            (left.as_ref(), right.as_ref())
                            && matches!(
                                op,
                                CompareOp::Eq
                                    | CompareOp::Ne
                                    | CompareOp::LooseEq
                                    | CompareOp::LooseNe
                            )
                        {
                            for id in [a, b] {
                                if let Some(f) = self.facts.templates.get(&self.origin(*id))
                                    && !f.captures.is_empty()
                                {
                                    return Err(Diagnostic::unsupported(format!(
                                        "cannot compare capturing function '{}'",
                                        f.name
                                    ))
                                    .with_fallback_span(
                                        self.env.metadata.locations.function_span(&f.name),
                                    ));
                                }
                            }
                            *expr = Expr::Bool(
                                (self.origin(*a) == self.origin(*b))
                                    == matches!(op, CompareOp::Eq | CompareOp::LooseEq),
                            );
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
            fn statement(&mut self, stmt: &mut Stmt) -> Result<()> {
                if let Stmt::Let {
                    id,
                    name,
                    init: Some(value),
                    ..
                } = stmt
                {
                    if self.facts.invoked.contains(id) {
                        self.facts.lift(value, Some(name));
                    }
                    if let Expr::LocalGet(other) = value
                        && let Some(target) = self.facts.aliases.get(other)
                    {
                        *value = Expr::FuncRef(*target);
                    }
                    match value {
                        Expr::FuncRef(target)
                        | Expr::Closure {
                            func_id: target, ..
                        } => {
                            self.facts.aliases.insert(*id, *target);
                        }
                        _ => {}
                    }
                }
                walk::visit_stmt(stmt, self)
            }
            fn expression(&mut self, expr: &mut Expr) -> Result<()> {
                if let Expr::Sequence(parts) = expr
                    && let Some(span) = self.env.metadata.locations.operator_span(parts)
                {
                    return walk::visit_expr(expr, self).map_err(|error| error.with_span(span));
                }
                if let Expr::Compare { left, right, .. } = expr {
                    self.facts.lift(left, None);
                    self.facts.lift(right, None);
                }
                if let Expr::Call {
                    callee,
                    args,
                    byte_offset,
                    ..
                } = expr
                {
                    if let Expr::LocalGet(id) = callee.as_ref()
                        && let Some(target) = self.facts.aliases.get(id)
                    {
                        **callee = Expr::FuncRef(*target);
                    }
                    if matches!(callee.as_ref(), Expr::FuncRef(id) if self.facts.templates.contains_key(id))
                    {
                        for arg in args {
                            self.facts.lift(arg, None);
                            if matches!(arg, Expr::Closure { .. }) {
                                return Err(self.env.metadata.locations.annotate_call(Diagnostic::unsupported("a callback that captures this or new.target requires its original receiver"), *byte_offset));
                            }
                        }
                    }
                }
                walk::visit_expr(expr, self)
            }
        }
        Resolve { facts: self, env }.body(body)
    }
}
