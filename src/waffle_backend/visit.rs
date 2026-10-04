//! Expression traversal for backend dependency and contract checks.

use perry_hir::ir::{Expr, Function, Stmt};
use perry_hir::walker::walk_expr_children;

pub(crate) fn contains_type(
    ty: &perry_hir::types::Type,
    predicate: fn(&perry_hir::types::Type) -> bool,
) -> bool {
    use perry_hir::types::Type;
    if predicate(ty) {
        return true;
    }
    match ty {
        Type::Array(inner) | Type::Promise(inner) => contains_type(inner, predicate),
        Type::Union(types)
        | Type::Tuple(types)
        | Type::Generic {
            type_args: types, ..
        } => types.iter().any(|ty| contains_type(ty, predicate)),
        Type::Object(record) => {
            record
                .properties
                .values()
                .any(|field| contains_type(&field.ty, predicate))
                || record
                    .index_signature
                    .as_deref()
                    .is_some_and(|ty| contains_type(ty, predicate))
        }
        _ => false,
    }
}

pub(crate) fn visit_function_expressions(function: &Function, visitor: &mut impl FnMut(&Expr)) {
    for parameter in &function.params {
        if let Some(default) = &parameter.default {
            visit_expression(default, visitor);
        }
    }
    visit_statements(&function.body, visitor);
}

pub(crate) fn visit_statements(statements: &[Stmt], visitor: &mut dyn FnMut(&Expr)) {
    visit_statement_nodes(statements, &mut |statement| match statement {
        Stmt::Expr(expression) | Stmt::Throw(expression) => visit_expression(expression, visitor),
        Stmt::Let { init, .. } | Stmt::Return(init) => {
            if let Some(expression) = init {
                visit_expression(expression, visitor);
            }
        }
        Stmt::If { condition, .. }
        | Stmt::While { condition, .. }
        | Stmt::DoWhile { condition, .. } => visit_expression(condition, visitor),
        Stmt::For {
            condition, update, ..
        } => {
            for expression in condition.iter().chain(update) {
                visit_expression(expression, visitor);
            }
        }
        Stmt::Switch {
            discriminant,
            cases,
        } => {
            visit_expression(discriminant, visitor);
            for case in cases {
                if let Some(test) = &case.test {
                    visit_expression(test, visitor);
                }
            }
        }
        _ => {}
    });
}

pub(crate) fn visit_statement_nodes(statements: &[Stmt], visitor: &mut dyn FnMut(&Stmt)) {
    for statement in statements {
        visitor(statement);
        match statement {
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                visit_statement_nodes(then_branch, visitor);
                if let Some(branch) = else_branch {
                    visit_statement_nodes(branch, visitor);
                }
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => {
                visit_statement_nodes(body, visitor)
            }
            Stmt::For { init, body, .. } => {
                if let Some(init) = init {
                    visit_statement_nodes(std::slice::from_ref(init), visitor);
                }
                visit_statement_nodes(body, visitor);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                visit_statement_nodes(body, visitor);
                if let Some(catch) = catch {
                    visit_statement_nodes(&catch.body, visitor);
                }
                if let Some(finally) = finally {
                    visit_statement_nodes(finally, visitor);
                }
            }
            Stmt::Labeled { body, .. } => {
                visit_statement_nodes(std::slice::from_ref(body), visitor)
            }
            Stmt::Switch { cases, .. } => {
                for case in cases {
                    visit_statement_nodes(&case.body, visitor);
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn visit_statement_nodes_mut(input: &mut [Stmt], visitor: &mut impl FnMut(&mut Stmt)) {
    let mut statements: Vec<_> = input.iter_mut().collect();
    while let Some(statement) = statements.pop() {
        visitor(statement);
        match statement {
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                statements.extend(then_branch);
                statements.extend(else_branch.iter_mut().flatten());
            }
            Stmt::While { body, .. } | Stmt::DoWhile { body, .. } => statements.extend(body),
            Stmt::For { init, body, .. } => {
                statements.extend(init.iter_mut().map(Box::as_mut));
                statements.extend(body);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                statements.extend(body);
                statements.extend(catch.iter_mut().flat_map(|catch| &mut catch.body));
                statements.extend(finally.iter_mut().flatten());
            }
            Stmt::Switch { cases, .. } => {
                statements.extend(cases.iter_mut().flat_map(|case| &mut case.body))
            }
            Stmt::Labeled { body, .. } => statements.push(body),
            _ => {}
        }
    }
}

pub(crate) fn visit_expression(expression: &Expr, visitor: &mut dyn FnMut(&Expr)) {
    visitor(expression);
    walk_expr_children(expression, &mut |child| visit_expression(child, visitor));
    if let Expr::Closure { body, .. } = expression {
        visit_statements(body, visitor);
    }
}
