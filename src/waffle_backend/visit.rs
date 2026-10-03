//! Expression traversal for backend dependency and contract checks.

use perry_hir::ir::{Expr, Function, Stmt};
use perry_hir::walker::walk_expr_children;

pub(crate) fn visit_function_expressions(function: &Function, visitor: &mut impl FnMut(&Expr)) {
    for parameter in &function.params {
        if let Some(default) = &parameter.default {
            visit_expression(default, visitor);
        }
    }
    visit_statements(&function.body, visitor);
}

pub(crate) fn visit_statements(statements: &[Stmt], visitor: &mut dyn FnMut(&Expr)) {
    for statement in statements {
        match statement {
            Stmt::Expr(expression) | Stmt::Throw(expression) => {
                visit_expression(expression, visitor)
            }
            Stmt::Let { init, .. } | Stmt::Return(init) => {
                if let Some(expression) = init {
                    visit_expression(expression, visitor);
                }
            }
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => {
                visit_expression(condition, visitor);
                visit_statements(then_branch, visitor);
                if let Some(branch) = else_branch {
                    visit_statements(branch, visitor);
                }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                visit_expression(condition, visitor);
                visit_statements(body, visitor);
            }
            Stmt::For {
                init,
                condition,
                update,
                body,
            } => {
                if let Some(init) = init {
                    visit_statements(std::slice::from_ref(init), visitor);
                }
                for expression in condition.iter().chain(update) {
                    visit_expression(expression, visitor);
                }
                visit_statements(body, visitor);
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                visit_statements(body, visitor);
                if let Some(catch) = catch {
                    visit_statements(&catch.body, visitor);
                }
                if let Some(finally) = finally {
                    visit_statements(finally, visitor);
                }
            }
            Stmt::Labeled { body, .. } => visit_statements(std::slice::from_ref(body), visitor),
            Stmt::Switch {
                discriminant,
                cases,
            } => {
                visit_expression(discriminant, visitor);
                for case in cases {
                    if let Some(test) = &case.test {
                        visit_expression(test, visitor);
                    }
                    visit_statements(&case.body, visitor);
                }
            }
            Stmt::Break
            | Stmt::Continue
            | Stmt::LabeledBreak(_)
            | Stmt::LabeledContinue(_)
            | Stmt::PreallocateBoxes(_)
            | Stmt::PreallocateTdzBoxes(_)
            | Stmt::ReleaseBoxes(_) => {}
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
