//! Recognize Perry's compiler-owned dense-array iterator records.
use super::*;

#[derive(Default)]
pub(crate) struct Iteration {
    guards: HashSet<LocalId>,
    released: HashSet<LocalId>,
    positions: HashMap<LocalId, usize>,
}

pub(crate) enum RecordExpr<'a> {
    Value(&'a Expr),
    RequireObject(&'a Expr),
    Length(&'a Expr),
    Close,
}

pub(crate) enum Availability<'a> {
    Test(&'a Expr),
    Known(bool),
}

pub(crate) struct Element<'a> {
    pub(crate) id: LocalId,
    pub(crate) value: &'a Expr,
    pub(crate) advance: Option<&'a Expr>,
    pub(crate) done: LocalId,
    pub(crate) condition: &'a Expr,
    pub(crate) missing: &'a Expr,
    pub(crate) available: Availability<'a>,
    pub(crate) rest: &'a [Stmt],
}

fn runtime(expr: &Expr) -> Option<(&str, &[Expr])> {
    match expr {
        Expr::NativeMethodCall {
            module,
            class_name: None,
            object: None,
            method,
            args,
        } if module == "__perry_runtime" => Some((method, args)),
        _ => None,
    }
}

impl Iteration {
    pub(crate) fn register_binding(&mut self, id: LocalId, name: &str, init: Option<&Expr>) {
        if matches!(
            init.and_then(runtime),
            Some(("arrayRecordIndex", [Expr::Integer(0)]))
        ) {
            self.positions.insert(id, 0);
        }
        if name.starts_with("__iterator_record_")
            || name.starts_with("__iterator_literal_")
            || init.is_some_and(|expr| matches!(runtime(expr), Some(("arrayRecordPayload", _))))
            || matches!(init, Some(Expr::Conditional { condition, .. }) if self.is_guard(condition))
        {
            self.released.insert(id);
        }
    }

    pub(crate) fn guard<'a>(&mut self, id: LocalId, expr: &'a Expr) -> Option<Option<&'a Expr>> {
        let ("arrayRecordNeedsIterator", args) = runtime(expr)? else {
            return None;
        };
        let source = match args {
            [] => None,
            [Expr::LocalSet(id, source), ..] if matches!(&**source, Expr::LocalGet(other) if other == id) => {
                Some(&**source)
            }
            _ => return None,
        };
        self.guards.insert(id);
        Some(source)
    }

    pub(crate) fn is_guard(&self, expr: &Expr) -> bool {
        match expr {
            Expr::LocalGet(id) => self.guards.contains(id),
            Expr::Compare {
                op: CompareOp::Eq,
                left,
                right,
            } if matches!(&**right, Expr::Bool(true)) => self.is_guard(left),
            _ => false,
        }
    }

    pub(crate) fn expression<'a>(&self, expr: &'a Expr) -> Option<RecordExpr<'a>> {
        if let Expr::Conditional {
            condition,
            else_expr,
            ..
        } = expr
            && self.is_guard(condition)
        {
            return Some(RecordExpr::Value(else_expr));
        }
        Some(match runtime(expr)? {
            ("arrayRecordIndex", [value]) => RecordExpr::Value(value),
            ("requireObjectCoercible", [value, ..]) => RecordExpr::RequireObject(value),
            ("arrayRecordLength", [source]) => RecordExpr::Length(source),
            ("arrayRecordPayload", [_, Expr::LocalSet(_, value), _]) => RecordExpr::Value(value),
            ("arrayRecordClose", _) => RecordExpr::Close,
            _ => return None,
        })
    }

    pub(crate) fn index(&self, expr: &Expr) -> Option<usize> {
        let ("arrayRecordIndex", [Expr::LocalGet(id)]) = runtime(expr)? else {
            return None;
        };
        self.positions.get(id).copied()
    }

    pub(crate) fn advance(&mut self, id: LocalId) {
        if let Some(position) = self.positions.get_mut(&id) {
            *position += 1;
        }
    }

    pub(crate) fn is_release(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::LocalSet(id, value) if self.released.contains(id) && matches!(&**value, Expr::Undefined))
    }

    pub(crate) fn for_of_loop(&self, condition: Option<&Expr>) -> Option<(LocalId, LocalId)> {
        let Some(Expr::Compare {
            op: CompareOp::Lt,
            left,
            right,
        }) = condition
        else {
            return None;
        };
        let (
            Expr::LocalGet(index),
            Expr::PropertyGet {
                object, property, ..
            },
        ) = (left.as_ref(), right.as_ref())
        else {
            return None;
        };
        if property != "length" {
            return None;
        }
        let Expr::LocalGet(items) = object.as_ref() else {
            return None;
        };
        Some((*index, *items))
    }

    pub(crate) fn element<'a>(&self, statements: &'a [Stmt]) -> Option<Element<'a>> {
        let [
            Stmt::Let {
                id,
                init: Some(Expr::Undefined),
                ..
            },
            Stmt::If {
                condition,
                then_branch,
                else_branch: Some(pull),
            },
            rest @ ..,
        ] = statements
        else {
            return None;
        };
        let Expr::Compare {
            op: CompareOp::Eq,
            left,
            right,
        } = condition
        else {
            return None;
        };
        let Expr::LocalGet(done) = &**left else {
            return None;
        };
        if !matches!(&**right, Expr::Bool(true)) {
            return None;
        }
        let [Stmt::Expr(Expr::LocalSet(target, missing))] = then_branch.as_slice() else {
            return None;
        };
        if target != id || !matches!(&**missing, Expr::Undefined) {
            return None;
        }
        let indexed = pull.iter().find_map(|statement| match statement {
            Stmt::Let {
                init:
                    Some(Expr::Conditional {
                        condition,
                        else_expr,
                        ..
                    }),
                ..
            } if self.is_guard(condition) => Some(&**else_expr),
            _ => None,
        })?;
        let (success, test) = match indexed {
            Expr::Conditional {
                condition,
                then_expr,
                ..
            } => (&**then_expr, Some(&**condition)),
            other => (other, None),
        };
        let Expr::Sequence(steps) = success else {
            return None;
        };
        let [Expr::LocalSet(target, value), remaining @ ..] = steps.as_slice() else {
            return None;
        };
        if target != id {
            return None;
        }
        let advance = remaining
            .first()
            .filter(|expr| !matches!(expr, Expr::Bool(_)));
        let available = match test {
            Some(test) => Availability::Test(test),
            None => Availability::Known(matches!(steps.last(), Some(Expr::Bool(false)))),
        };
        Some(Element {
            id: *id,
            value,
            advance,
            rest,
            done: *done,
            condition,
            missing,
            available,
        })
    }
}
