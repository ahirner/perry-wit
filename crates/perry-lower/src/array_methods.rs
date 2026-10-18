//! Dense-array methods lowered inline: callbacks of `map`/`filter`, and `indexOf`/`includes`.
use super::*;
use perry_mir::{ArrayAtOp, OptionalType};

mod callback;
mod index;
mod loops;
mod search;
mod transform;
#[derive(Clone, Copy)]
pub(super) enum Direction {
    Forward,
    Reverse,
}
#[derive(Clone, Copy)]
pub(super) enum FindResult {
    Value,
    Index,
}
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SearchResult {
    Includes,
    Index,
}
#[derive(Clone, Copy)]
pub(super) enum Transform {
    Map,
    Filter,
    FlatMap,
}
#[derive(Clone, Copy)]
pub(super) enum Quantifier {
    Some,
    Every,
}
#[derive(Clone, Copy)]
pub(super) enum Mutation {
    InPlace,
    Copy,
}

enum Holes {
    Skip,
    Visit(TypeHandle),
}
struct ArrayIteration {
    array: Value,
    start: Value,
    limit: Value,
    direction: Direction,
    holes: Holes,
}

/// The frame of an inline array callback being lowered: its `return`s store into `slot` and
/// branch to `done` instead of leaving the enclosing function.
pub(super) struct CallbackReturn {
    pub(super) done: Ptr<BasicBlock>,
    /// The result slot, from the declared return type or the first typed `return`.
    pub(super) slot: Option<Value>,
    /// Whether the callback may complete without a value, making its result optional.
    pub(super) optional: bool,
    /// Blocks ending in `return;` before the slot existed; they store `undefined` afterwards.
    pub(super) empty_returns: Vec<Ptr<BasicBlock>>,
}

// How a statement list may complete, as a bit set.
pub(super) const FALLTHROUGH: u8 = 1;
const RETURN: u8 = 2;
const BREAK: u8 = 4;
const CONTINUE: u8 = 8;
const LABELLED: u8 = 16;
// Return-type inference also sees syntactically present returns on unreachable paths.
pub(super) const EMPTY_RETURN: u8 = 32;

/// The ways `statements` may complete; a callback whose body may fall through returns
/// `undefined` on that path.
pub(super) fn completions(statements: &[Stmt]) -> u8 {
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
                (body & (RETURN | EMPTY_RETURN | LABELLED))
                    | if condition
                        .as_ref()
                        .is_none_or(|c| matches!(c, Expr::Bool(true)))
                        && body & BREAK == 0
                    {
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

pub(super) fn returns_optional(statements: &[Stmt]) -> bool {
    completions(statements) & (FALLTHROUGH | EMPTY_RETURN) != 0
}

/// Insert `operations`, in order, before the terminator of an already closed `block`: for
/// values whose type is known only after later code was lowered.
pub(super) fn insert_before_terminator(
    ctx: &mut Context,
    block: Ptr<BasicBlock>,
    operations: impl IntoIterator<Item = Ptr<Operation>>,
) -> Result<()> {
    let terminator = block
        .deref(ctx)
        .get_terminator(ctx)
        .context("a closed block ends in a terminator")?;
    for operation in operations {
        operation.insert_before(ctx, terminator);
    }
    Ok(())
}
