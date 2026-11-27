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

fn compared_to_true(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Compare {
            op: CompareOp::Eq,
            left,
            right,
            ..
        } if matches!(&**right, Expr::Bool(true)) => Some(left),
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

    fn is_guard(&self, expr: &Expr) -> bool {
        match expr {
            Expr::LocalGet(id) => self.guard(*id).is_some(),
            _ => compared_to_true(expr).is_some_and(|left| self.is_guard(left)),
        }
    }

    pub(crate) fn dense_statements<'a>(&self, stmt: &'a Stmt) -> Option<&'a [Stmt]> {
        let Stmt::If {
            condition,
            then_branch,
            else_branch,
        } = stmt
        else {
            return None;
        };
        match condition {
            Expr::Unary {
                op: perry_hir::UnaryOp::Not,
                operand,
            } if matches!(&**operand, Expr::ArrayIterationPatched) => Some(then_branch),
            condition if self.is_guard(condition) => {
                Some(else_branch.as_deref().unwrap_or_default())
            }
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
        if self.is_guard(condition) {
            return Some(else_expr);
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
                if self.is_guard(guard) =>
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

    pub(crate) fn index_rest_value(&mut self, expr: &Expr) {
        if let Some(RecordExpr::RestAppend(_, Expr::LocalGet(id))) = self.expression(expr)
            && let Some(local) = self.index(*id)
            && self.rows[local].flags.contains(LocalFlags::GENERATED)
        {
            // Exhaustion writes undefined and exits before appending this item.
            self.rows[local].flags.insert(LocalFlags::DEFERRED);
            self.rows[local].iteration = Some(Binding::Temporary);
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
        let Expr::LocalGet(done) = compared_to_true(condition)? else {
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
