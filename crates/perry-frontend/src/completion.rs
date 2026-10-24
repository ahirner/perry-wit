//! Return completion facts shared by inference and elaboration.
use perry_hir::ir::{Expr, Stmt};
const FALLTHROUGH: u8 = 1;
const RETURN: u8 = 2;
const BREAK: u8 = 4;
const CONTINUE: u8 = 8;
const LABELLED: u8 = 16;
// Return-type inference also sees syntactically present returns on unreachable paths.
const EMPTY_RETURN: u8 = 32;

/// Compute the completion mask of `statements` so a callback that can fall through yields
/// `undefined` on that path.
fn completions(statements: &[Stmt]) -> u8 {
    let mut result = FALLTHROUGH;
    for statement in statements {
        let next = match statement {
            Stmt::Return(None | Some(Expr::Undefined)) => RETURN | EMPTY_RETURN,
            Stmt::Return(_) | Stmt::Throw(_) => RETURN,
            Stmt::Break => BREAK,
            Stmt::Continue => CONTINUE,
            Stmt::LabeledBreak(_) | Stmt::LabeledContinue(_) => LABELLED,
            Stmt::If {
                then_branch,
                else_branch,
                ..
            } => {
                completions(then_branch)
                    | else_branch
                        .as_ref()
                        .map_or(FALLTHROUGH, |body| completions(body))
            }
            Stmt::Switch { cases, .. } => {
                let mut tail = FALLTHROUGH;
                let mut branches = if cases.iter().any(|case| case.test.is_none()) {
                    0
                } else {
                    FALLTHROUGH
                };
                for case in cases.iter().rev() {
                    let body = completions(&case.body);
                    tail = (body & !FALLTHROUGH) | if body & FALLTHROUGH != 0 { tail } else { 0 };
                    branches |= tail;
                }
                (branches & !BREAK)
                    | if branches & BREAK != 0 {
                        FALLTHROUGH
                    } else {
                        0
                    }
            }
            Stmt::While { condition, body } | Stmt::DoWhile { condition, body } => {
                let body = completions(body);
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if matches!(condition, Expr::Bool(true)) && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::For {
                condition, body, ..
            } => {
                let body = completions(body);
                let inf = condition
                    .as_ref()
                    .is_none_or(|c| matches!(c, Expr::Bool(true)));
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if inf && body & BREAK == 0 {
                        0
                    } else {
                        FALLTHROUGH
                    }
            }
            Stmt::Try {
                body,
                catch,
                finally,
            } => {
                let body =
                    completions(body) | catch.as_ref().map_or(0, |catch| completions(&catch.body));
                let final_state = finally
                    .as_ref()
                    .map_or(FALLTHROUGH, |body| completions(body));
                (body & EMPTY_RETURN)
                    | (final_state & !FALLTHROUGH)
                    | if final_state & FALLTHROUGH != 0 {
                        body
                    } else {
                        0
                    }
            }
            Stmt::Labeled { body, .. } => {
                FALLTHROUGH | (completions(std::slice::from_ref(body)) & EMPTY_RETURN)
            }
            _ => FALLTHROUGH,
        };
        result = (result & !FALLTHROUGH)
            | if result & FALLTHROUGH != 0 {
                next
            } else {
                next & EMPTY_RETURN
            };
    }
    result
}

pub fn returns_optional(statements: &[Stmt]) -> bool {
    completions(statements) & (FALLTHROUGH | EMPTY_RETURN) != 0
}
