//! Adapt Perry's async state machines to guest cells and Promise continuations.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Result, ensure};
use perry_hir::ir::{Expr, Module, Stmt};
use perry_hir::types::{LocalId, Type};
use perry_hir::walker::walk_expr_children_mut;
use perry_parser::swc_ecma_ast::{
    ArrowExpr, CallExpr, Callee, ClassMethod, Expr as AstExpr, FnExpr, Function as AstFunction,
    MemberProp, Module as AstModule, NewExpr,
};
use swc_ecma_visit::{Visit, VisitWith};

/// Reuses the pinned state-machine transform without a JavaScript fallback host.
pub(super) fn lower(module: &mut Module, ast: &AstModule) -> Result<()> {
    ensure!(
        module.classes.iter().all(|class| class
            .constructor
            .iter()
            .chain(&class.methods)
            .chain(&class.static_methods)
            .chain(class.getters.iter().map(|(_, method)| method))
            .chain(class.setters.iter().map(|(_, method)| method))
            .chain(class.computed_members.iter().map(|member| &member.function))
            .all(|method| !method.is_async)),
        "Async class methods are not supported yet"
    );
    if !module.functions.iter().any(|function| function.is_async) {
        return Ok(());
    }
    let mut forms = UnsupportedForms(None);
    ast.visit_with(&mut forms);
    if let Some(error) = forms.0 {
        anyhow::bail!(error);
    }
    for function in &module.functions {
        if function.is_async {
            ensure!(
                function.captures.is_empty(),
                "Named async functions with captures are not supported yet"
            );
            ensure!(
                function.params.iter().all(|parameter| !parameter.is_rest
                    && parameter.default.is_none()
                    && parameter.arguments_object.is_none()),
                "Async functions with rest, default, or arguments parameters are not supported yet"
            );
        }
    }
    perry_transform::transform_async_to_generator(module);
    for function in &mut module.functions {
        if function.is_async && !function.is_generator {
            function.is_async = false;
            function.is_generator = true;
            function.was_plain_async = true;
        }
    }
    perry_transform::transform_generators(module);
    for function in &mut module.functions {
        if function.was_plain_async {
            adapt_body(
                &mut function.body,
                &HashSet::new(),
                function.params.iter().map(|parameter| parameter.id),
            )?;
        }
    }
    module
        .init
        .push(Stmt::Expr(Expr::String("__needs_async__".into())));
    Ok(())
}

struct UnsupportedForms(Option<&'static str>);

impl Visit for UnsupportedForms {
    fn visit_arrow_expr(&mut self, arrow: &ArrowExpr) {
        if arrow.is_async {
            self.0 = Some("Async and generator guest closures are not supported yet");
        }
        arrow.visit_children_with(self);
    }

    fn visit_fn_expr(&mut self, expression: &FnExpr) {
        if expression.function.is_async {
            self.0 = Some("Async and generator guest closures are not supported yet");
        }
        expression.visit_children_with(self);
    }

    fn visit_function(&mut self, function: &AstFunction) {
        if function.is_generator {
            self.0 = Some("Generator guest functions are not supported yet");
        }
        function.visit_children_with(self);
    }

    fn visit_class_method(&mut self, method: &ClassMethod) {
        if method.function.is_async {
            self.0 = Some("Async class methods are not supported yet");
        }
        method.visit_children_with(self);
    }

    fn visit_new_expr(&mut self, expression: &NewExpr) {
        if matches!(expression.callee.as_ref(), AstExpr::Ident(identifier) if identifier.sym == "Promise")
        {
            self.0 = Some("Promise constructors in guest async programs are not supported yet");
        }
        expression.visit_children_with(self);
    }

    fn visit_call_expr(&mut self, call: &CallExpr) {
        if let Callee::Expr(callee) = &call.callee
            && let AstExpr::Member(member) = callee.as_ref()
            && matches!(member.obj.as_ref(), AstExpr::Ident(identifier) if identifier.sym == "Promise")
            && matches!(&member.prop, MemberProp::Ident(_))
        {
            self.0 = Some("Promise static methods in guest async programs are not supported yet");
        }
        call.visit_children_with(self);
    }
}

/// Exposes declarations hidden inside labels/do-while to the Wasm local collector.
fn adapt_body(
    body: &mut Vec<Stmt>,
    inherited: &HashSet<LocalId>,
    parameters: impl Iterator<Item = LocalId>,
) -> Result<()> {
    let mut boxes = HashSet::new();
    let mut locals = BTreeSet::new();
    collect_frame(body, &mut boxes, &mut locals);
    boxes.extend(inherited);
    for parameter in parameters {
        locals.remove(&parameter);
    }
    locals.retain(|id| !boxes.contains(id));
    for statement in body.iter_mut() {
        adapt_statement(statement, &boxes)?;
    }
    body.splice(
        0..0,
        locals.into_iter().map(|id| Stmt::Let {
            id,
            name: format!("__guest_local_{id}"),
            ty: Type::Any,
            mutable: true,
            init: None,
        }),
    );
    Ok(())
}

fn collect_frame(body: &[Stmt], boxes: &mut HashSet<LocalId>, locals: &mut BTreeSet<LocalId>) {
    for statement in body {
        match statement {
            Stmt::PreallocateBoxes(ids) => boxes.extend(ids),
            Stmt::Let { id, .. } => {
                locals.insert(*id);
            }
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                collect_frame(then_branch, boxes, locals);
                if let Some(body) = else_branch {
                    collect_frame(body, boxes, locals);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                collect_frame(body, boxes, locals)
            }
            Stmt::For { init, body, .. } => {
                if let Some(statement) = init {
                    collect_frame(std::slice::from_ref(statement), boxes, locals);
                }
                collect_frame(body, boxes, locals);
            }
            Stmt::Labeled { body, .. } => collect_frame(std::slice::from_ref(body), boxes, locals),
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                collect_frame(body, boxes, locals);
                if let Some(catch) = catch {
                    if let Some((id, _)) = catch.param {
                        locals.insert(id);
                    }
                    collect_frame(&catch.body, boxes, locals);
                }
                if let Some(body) = finally {
                    collect_frame(body, boxes, locals);
                }
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    collect_frame(&case.body, boxes, locals);
                }
            }
            _ => {}
        }
    }
}

fn adapt_statement(statement: &mut Stmt, boxes: &HashSet<LocalId>) -> Result<()> {
    match statement {
        Stmt::PreallocateBoxes(ids) => {
            let allocations = ids
                .iter()
                .map(|&id| Stmt::Let {
                    id,
                    name: format!("__guest_cell_{id}"),
                    ty: Type::Any,
                    mutable: false,
                    init: Some(bridge("async_cell_new", vec![])),
                })
                .collect();
            *statement = Stmt::If {
                condition: Expr::Bool(true),
                then_branch: allocations,
                else_branch: None,
            };
            return Ok(());
        }
        Stmt::Let { id, init, .. } if boxes.contains(id) => {
            let mut value = init.take().unwrap_or(Expr::Undefined);
            adapt_expression(&mut value, boxes)?;
            *statement = Stmt::Expr(bridge("async_cell_set", vec![Expr::LocalGet(*id), value]));
            return Ok(());
        }
        Stmt::ReleaseBoxes(_) => {
            *statement = Stmt::Expr(Expr::Undefined);
        }
        Stmt::Expr(value)
        | Stmt::Return(Some(value))
        | Stmt::Throw(value)
        | Stmt::Let {
            init: Some(value), ..
        } => adapt_expression(value, boxes)?,
        Stmt::If {
            condition,
            then_branch,
            else_branch,
        } => {
            adapt_expression(condition, boxes)?;
            for statement in then_branch
                .iter_mut()
                .chain(else_branch.iter_mut().flatten())
            {
                adapt_statement(statement, boxes)?;
            }
        }
        Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
            adapt_expression(condition, boxes)?;
            for statement in body {
                adapt_statement(statement, boxes)?;
            }
        }
        Stmt::For {
            init,
            condition,
            update,
            body,
        } => {
            if let Some(statement) = init {
                adapt_statement(statement, boxes)?;
            }
            for value in condition.iter_mut().chain(update.iter_mut()) {
                adapt_expression(value, boxes)?;
            }
            for statement in body {
                adapt_statement(statement, boxes)?;
            }
        }
        Stmt::Labeled { body, .. } => adapt_statement(body, boxes)?,
        Stmt::Try {
            body,
            catch,
            finally,
        } => {
            if let Some(catch) = catch {
                ensure!(
                    catch
                        .param
                        .as_ref()
                        .is_none_or(|(id, _)| !boxes.contains(id)),
                    "Async catch binding requires cell adaptation"
                );
                for statement in &mut catch.body {
                    adapt_statement(statement, boxes)?;
                }
            }
            for statement in body.iter_mut().chain(finally.iter_mut().flatten()) {
                adapt_statement(statement, boxes)?;
            }
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            adapt_expression(discriminant, boxes)?;
            for case in cases {
                if let Some(value) = &mut case.test {
                    adapt_expression(value, boxes)?;
                }
                for statement in &mut case.body {
                    adapt_statement(statement, boxes)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn adapt_expression(expression: &mut Expr, boxes: &HashSet<LocalId>) -> Result<()> {
    match expression {
        Expr::LocalGet(id) if boxes.contains(id) => {
            *expression = bridge("async_cell_get", vec![Expr::LocalGet(*id)]);
            return Ok(());
        }
        Expr::LocalSet(id, value) if boxes.contains(id) => {
            adapt_expression(value, boxes)?;
            let value = std::mem::replace(value.as_mut(), Expr::Undefined);
            *expression = bridge("async_cell_set", vec![Expr::LocalGet(*id), value]);
            return Ok(());
        }
        Expr::Update { id, op, prefix } if boxes.contains(id) => {
            let delta = if *op == perry_hir::ir::UpdateOp::Increment {
                1.0
            } else {
                -1.0
            };
            *expression = bridge(
                "async_cell_update",
                vec![
                    Expr::LocalGet(*id),
                    Expr::Number(delta),
                    Expr::Bool(*prefix),
                ],
            );
            return Ok(());
        }
        Expr::Closure {
            body,
            mutable_captures,
            params,
            captures,
            ..
        } => {
            mutable_captures.retain(|id| !boxes.contains(id));
            adapt_body(
                body,
                boxes,
                params
                    .iter()
                    .map(|parameter| parameter.id)
                    .chain(captures.iter().copied())
                    .chain(mutable_captures.iter().copied()),
            )?;
        }
        _ => {}
    }
    let mut result = Ok(());
    walk_expr_children_mut(expression, &mut |child| {
        if result.is_ok() {
            result = adapt_expression(child, boxes);
        }
    });
    result?;
    let replacement = match expression {
        Expr::IterResultSet(value, done) => Some(bridge(
            "async_iter_set",
            vec![
                std::mem::replace(value.as_mut(), Expr::Undefined),
                Expr::Bool(*done),
            ],
        )),
        Expr::IterResultGetValue => Some(bridge("async_iter_value", vec![])),
        Expr::IterResultGetDone => Some(bridge("async_iter_done", vec![])),
        Expr::CurrentStepClosure => Some(bridge("async_current_step", vec![])),
        Expr::AsyncFirstCall { step_closure } => Some(bridge(
            "async_first_call",
            vec![std::mem::replace(step_closure.as_mut(), Expr::Undefined)],
        )),
        Expr::AsyncStepChain {
            value,
            step_closure,
        } => Some(bridge(
            "async_step_chain",
            vec![
                std::mem::replace(value.as_mut(), Expr::Undefined),
                std::mem::replace(step_closure.as_mut(), Expr::Undefined),
            ],
        )),
        Expr::AsyncStepDone { value, .. } => Some(bridge(
            "async_resolve",
            vec![std::mem::replace(value.as_mut(), Expr::Undefined)],
        )),
        Expr::Call { callee, args, .. } if matches!(callee.as_ref(), Expr::PropertyGet {object, property, ..} if matches!(object.as_ref(), Expr::GlobalGet(0)) && property == "reject") => {
            Some(bridge("async_reject", std::mem::take(args)))
        }
        _ => None,
    };
    if let Some(replacement) = replacement {
        *expression = replacement;
    }
    Ok(())
}

/// Standard bridge calls let the existing backend collect names and evaluate arguments.
fn bridge(name: &str, arguments: Vec<Expr>) -> Expr {
    Expr::Call {
        callee: Box::new(Expr::PropertyGet {
            object: Box::new(Expr::Undefined),
            property: name.into(),
            byte_offset: 0,
        }),
        args: arguments,
        type_args: Vec::new(),
        byte_offset: 0,
    }
}
