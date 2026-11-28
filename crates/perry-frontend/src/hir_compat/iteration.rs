//! Recognize Perry's compiler-owned dense-array iterator records.
use super::*;
use std::collections::HashMap;

use crate::engine::types::{IdxVec, LocalFlags, LocalIdx, LocalInfo};

#[derive(Default)]
pub(crate) struct Locals<'h> {
    pub(crate) rows: IdxVec<LocalIdx, LocalInfo<'h>>,
    pub(crate) ids: Vec<Option<LocalIdx>>,
    elements: Vec<Element<'h>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Binding<'h> {
    Guard(Option<&'h Expr>),
    Temporary,
    YieldedValue,
}

pub(crate) enum RecordExpr<'a> {
    Value(&'a Expr),
    RequireObject(&'a Expr),
    Length(&'a Expr),
    RestAppend(&'a Expr, &'a Expr),
    Close,
}

#[derive(Clone, Copy)]
pub(crate) enum Availability<'a> {
    Test(&'a Expr),
    Known(bool),
}

#[derive(Clone, Copy)]
pub(crate) struct Element<'a> {
    pub(crate) id: LocalId,
    pub(crate) value: &'a Expr,
    pub(crate) advance: Option<&'a Expr>,
    pub(crate) done: LocalId,
    pub(crate) condition: &'a Expr,
    pub(crate) available: Availability<'a>,
}

fn runtime(expr: &Expr) -> Option<(&str, &[Expr])> {
    match expr {
        Expr::NativeMethodCall {
            module,
            class_name: None,
            object: None,
            method,
            args,
            ..
        } if module == "__perry_runtime" => Some((method, args)),
        _ => None,
    }
}

fn compared_bool(expr: &Expr) -> Option<(&Expr, bool)> {
    match expr {
        Expr::Compare {
            op: CompareOp::Eq,
            left,
            right,
            ..
        } if let Expr::Bool(value) = **right => Some((left, value)),
        _ => None,
    }
}

impl<'h> Locals<'h> {
    pub(crate) fn for_of<'a>(&self, init: Option<&'a Stmt>) -> Option<(LocalId, &'a Expr)> {
        let Stmt::Let {
            id,
            init: Some(source),
            ..
        } = init?
        else {
            return None;
        };
        self.row(*id)?
            .flags
            .contains(LocalFlags::FOR_OF)
            .then_some((*id, source))
    }

    pub(crate) fn index(&self, id: LocalId) -> Option<LocalIdx> {
        self.ids.get(id as usize).copied().flatten()
    }
    pub(crate) fn row(&self, id: LocalId) -> Option<&LocalInfo<'h>> {
        Some(&self.rows[self.index(id)?])
    }
    pub(crate) fn insert(&mut self, row: LocalInfo<'h>) -> LocalIdx {
        let id = row.id as usize;
        let index = self.rows.push(row);
        if id >= self.ids.len() {
            self.ids.resize(id + 1, None);
        }
        self.ids[id] = Some(index);
        index
    }

    pub(crate) fn bind(&mut self, local: LocalIdx, name: &str, init: Option<&'h Expr>) {
        let call = init.and_then(runtime);
        self.rows[local].iteration = match call {
            Some(("arrayRecordNeedsIterator", [])) => Some(Binding::Guard(None)),
            Some(("arrayRecordNeedsIterator", [Expr::LocalSet(local, source), ..])) if matches!(&**source, Expr::LocalGet(other) if other == local) => {
                Some(Binding::Guard(Some(source)))
            }
            _ if self.rows[local].flags.contains(LocalFlags::GENERATED)
                && (name.starts_with("__iterator_record_")
                    || name.starts_with("__iterator_literal_")
                    || matches!(call, Some(("arrayRecordPayload", _)))
                    || init.and_then(|expr| self.dense_branch(expr)).is_some()) =>
            {
                Some(Binding::Temporary)
            }
            _ => None,
        };
    }

    pub(crate) fn guard(&self, id: LocalId) -> Option<Option<&'h Expr>> {
        match self.row(id)?.iteration? {
            Binding::Guard(source) => Some(source),
            _ => None,
        }
    }

    fn dense_test(&self, expr: &Expr) -> Option<bool> {
        match expr {
            Expr::ArrayIterationPatched => Some(false),
            Expr::LocalGet(id) => self.guard(*id).map(|_| false),
            Expr::Unary {
                op: perry_hir::UnaryOp::Not,
                operand,
            } => Some(!self.dense_test(operand)?),
            _ if let Some((left, value)) = compared_bool(expr) => {
                Some(self.dense_test(left)? == value)
            }
            _ if matches!(
                runtime(expr),
                Some(("arrayRecordNeedsIterator", [Expr::LocalGet(_), ..]))
            ) =>
            {
                Some(false)
            }
            _ => None,
        }
    }

    pub(crate) fn dense_statements<'a>(&self, stmt: &'a Stmt) -> Option<&'a [Stmt]> {
        match stmt {
            // The builtin array iterator has no observable close operation.
            Stmt::Try {
                forof_completion: Some(_),
                body,
                ..
            } => Some(body),
            Stmt::If {
                condition,
                then_branch,
                else_branch,
            } => Some(if self.dense_test(condition)? {
                then_branch
            } else {
                else_branch.as_deref().unwrap_or_default()
            }),
            _ => None,
        }
    }

    fn dense_branch<'a>(&self, expr: &'a Expr) -> Option<&'a Expr> {
        let Expr::Conditional {
            condition,
            then_expr,
            else_expr,
            ..
        } = expr
        else {
            return None;
        };
        if let Some(test) = self.dense_test(condition) {
            return Some(if test { then_expr } else { else_expr });
        }
        let Expr::LocalGet(id) = &**else_expr else {
            return None;
        };
        let available = self.bound_element(*id)?.available;
        (matches!(available, Availability::Known(false))
            && crate::narrowing::DefinedGuard::of(condition)?.proves(false, else_expr))
        .then_some(then_expr)
    }

    pub(crate) fn expression<'a>(&self, expr: &'a Expr) -> Option<RecordExpr<'a>> {
        if let Some(value) = self.dense_branch(expr) {
            return Some(RecordExpr::Value(value));
        }
        Some(match runtime(expr)? {
            ("arrayRecordForBound" | "arrayRecordForUpdate", [guard, value, ..])
            | ("arrayRecordForValue", [guard, _, value])
                if self.dense_test(guard) == Some(false) =>
            {
                RecordExpr::Value(value)
            }
            ("arrayRecordIndex", [value]) => RecordExpr::Value(value),
            ("requireObjectCoercible", [value, ..]) => RecordExpr::RequireObject(value),
            ("arrayRecordLength", [source]) => RecordExpr::Length(source),
            ("iteratorRestAppend", [array, value]) => RecordExpr::RestAppend(array, value),
            ("arrayRecordPayload", [_, Expr::LocalSet(_, value), _]) => RecordExpr::Value(value),
            ("arrayRecordClose", _) => RecordExpr::Close,
            _ => return None,
        })
    }

    pub(crate) fn is_release(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::LocalSet(id, value) if self.row(*id).and_then(|r| r.iteration).is_some() && matches!(&**value, Expr::Undefined))
    }

    pub(crate) fn initializer<'a>(&self, id: LocalId, init: Option<&'a Expr>) -> Option<&'a Expr> {
        match (self.row(id).and_then(|row| row.iteration), init) {
            (Some(Binding::YieldedValue), Some(Expr::Undefined)) => None,
            (_, init) => init,
        }
    }

    pub(crate) fn index_iterator_value(&mut self, expr: &Expr) {
        let id = match runtime(expr) {
            Some(("iteratorRestAppend", [_, Expr::LocalGet(id)]))
            | Some(("arrayRecordForValue", [_, Expr::LocalGet(id), _])) => *id,
            _ => return,
        };
        if let Some(local) = self.index(id)
            && self.rows[local].flags.contains(LocalFlags::GENERATED)
        {
            // Initialization and release write undefined outside the yielded value's lifetime.
            self.rows[local].iteration = Some(Binding::YieldedValue);
        }
    }

    pub(crate) fn index_elements(
        &mut self,
        statements: &'h [Stmt],
        mut position: impl FnMut(&Expr, usize),
    ) {
        let mut ordinals = HashMap::<LocalId, usize>::new();
        for [previous, statement] in statements.array_windows() {
            let Some((element, binds)) = self.recognize_element(statement) else {
                continue;
            };
            if let Expr::IndexGet { index, .. } = element.value
                && matches!(
                    runtime(index),
                    Some(("arrayRecordIndex", [Expr::LocalGet(_)]))
                )
            {
                let next = ordinals.entry(element.done).or_default();
                position(index, *next);
                *next += 1;
            }
            if binds
                && matches!(previous, Stmt::Let {id, init: Some(Expr::Undefined), ..} if *id == element.id)
                && let Some(local) = self.index(element.id)
            {
                self.rows[local].element = Some(self.elements.len());
                self.elements.push(element);
            }
        }
    }

    pub(crate) fn element<'a>(&self, statements: &'a [Stmt]) -> Option<(Element<'h>, &'a [Stmt])> {
        let [Stmt::Let { id, .. }, Stmt::If { condition, .. }, rest @ ..] = statements else {
            return None;
        };
        let element = self.bound_element(*id)?;
        std::ptr::eq(condition, element.condition).then_some((*element, rest))
    }

    fn bound_element(&self, id: LocalId) -> Option<&Element<'h>> {
        self.elements.get(self.row(id)?.element?)
    }

    fn recognize_element(&self, stmt: &'h Stmt) -> Option<(Element<'h>, bool)> {
        let Stmt::If {
            condition,
            then_branch,
            else_branch: Some(pull),
        } = stmt
        else {
            return None;
        };
        let (Expr::LocalGet(done), true) = compared_bool(condition)? else {
            return None;
        };
        let [Stmt::Expr(Expr::LocalSet(id, missing))] = then_branch.as_slice() else {
            return None;
        };
        if !matches!(&**missing, Expr::Undefined) {
            return None;
        }
        let indexed = pull.iter().find_map(|stmt| match stmt {
            Stmt::Let {
                init: Some(init), ..
            } => self.dense_branch(init),
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
        let (first, remaining) = steps.split_first()?;
        let (value, binds) = match first {
            Expr::LocalSet(target, value) if target == id => (value.as_ref(), true),
            elision @ Expr::IndexGet { .. } => (elision, false),
            _ => return None,
        };
        let advance = remaining
            .first()
            .filter(|expr| !matches!(expr, Expr::Bool(_)));
        let available = match test {
            Some(test) => Availability::Test(test),
            None => Availability::Known(matches!(steps.last(), Some(Expr::Bool(false)))),
        };
        Some((
            Element {
                id: *id,
                value,
                advance,
                done: *done,
                condition,
                available,
            },
            binds,
        ))
    }
}
