//! Recognize Perry's compiler-owned dense-array iterator records.
use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct Iteration<'h> {
    bindings: HashMap<LocalId, (Binding<'h>, bool)>,
    elements: HashMap<LocalId, Element<'h>>,
}

enum Binding<'h> {
    Guard(Option<&'h Expr>),
    ObjectPattern(u32),
    Cursor,
    Temporary,
}

pub(crate) enum RecordExpr<'a> {
    Value(&'a Expr),
    RequireObject(&'a Expr),
    Length(&'a Expr),
    Close,
}

#[derive(Clone, Copy)]
pub(crate) enum Availability<'a> {
    Test(&'a Expr),
    Known(bool),
}

#[derive(Clone, Copy)]
pub(crate) struct Element<'a> {
    pub(crate) ty: &'a perry_hir::types::Type,
    pub(crate) id: LocalId,
    pub(crate) value: &'a Expr,
    pub(crate) advance: Option<&'a Expr>,
    pub(crate) done: LocalId,
    pub(crate) condition: &'a Expr,
    pub(crate) missing: &'a Expr,
    pub(crate) available: Availability<'a>,
}

const RUNTIME: &str = "__perry_runtime";
const REQUIRE_OBJECT: &str = "requireObjectCoercible";

pub(crate) fn is_object_coercibility_check(module: &str, method: &str) -> bool {
    module == RUNTIME && method == REQUIRE_OBJECT
}

fn runtime(expr: &Expr) -> Option<(&str, &[Expr])> {
    match expr {
        Expr::NativeMethodCall {
            module,
            class_name: None,
            object: None,
            method,
            args,
        } if module == RUNTIME => Some((method, args)),
        _ => None,
    }
}

fn compared_to_true(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Compare {
            op: CompareOp::Eq,
            left,
            right,
        } if matches!(&**right, Expr::Bool(true)) => Some(left),
        _ => None,
    }
}

impl<'h> Iteration<'h> {
    pub(crate) fn bind(&mut self, id: LocalId, name: &str, init: Option<&'h Expr>) {
        let call = init.and_then(runtime);
        let released = name.starts_with("__iterator_record_")
            || name.starts_with("__iterator_literal_")
            || matches!(call, Some(("arrayRecordPayload", _)))
            || matches!(init, Some(Expr::Conditional { condition, .. }) if self.is_guard(condition));
        let role = match call {
            Some(("arrayRecordNeedsIterator", [])) => Binding::Guard(None),
            Some(("arrayRecordNeedsIterator", [Expr::LocalSet(local, source), ..])) if matches!(&**source, Expr::LocalGet(other) if other == local) => {
                Binding::Guard(Some(source))
            }
            Some((REQUIRE_OBJECT, [_, Expr::Number(offset)])) => {
                Binding::ObjectPattern(*offset as u32)
            }
            Some(("arrayRecordIndex", [Expr::Integer(0)])) => Binding::Cursor,
            _ if released => Binding::Temporary,
            _ => return,
        };
        self.bindings.insert(id, (role, released));
    }

    pub(crate) fn object_pattern_offset(&self, receiver: &Expr) -> Option<u32> {
        let Expr::LocalGet(id) = receiver else {
            return None;
        };
        match self.bindings.get(id)? {
            (Binding::ObjectPattern(offset), _) => Some(*offset),
            _ => None,
        }
    }

    pub(crate) fn guard(&self, id: LocalId) -> Option<Option<&'h Expr>> {
        match self.bindings.get(&id)? {
            (Binding::Guard(source), _) => Some(*source),
            _ => None,
        }
    }

    pub(crate) fn is_cursor(&self, id: LocalId) -> bool {
        matches!(self.bindings.get(&id), Some((Binding::Cursor, _)))
    }

    pub(crate) fn is_guard(&self, expr: &Expr) -> bool {
        match expr {
            Expr::LocalGet(id) => self.guard(*id).is_some(),
            _ => compared_to_true(expr).is_some_and(|left| self.is_guard(left)),
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
            (REQUIRE_OBJECT, [value, ..]) => RecordExpr::RequireObject(value),
            ("arrayRecordLength", [source]) => RecordExpr::Length(source),
            ("arrayRecordPayload", [_, Expr::LocalSet(_, value), _]) => RecordExpr::Value(value),
            ("arrayRecordClose", _) => RecordExpr::Close,
            _ => return None,
        })
    }

    pub(crate) fn is_release(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::LocalSet(id, value) if self.bindings.get(id).is_some_and(|(_, released)| *released) && matches!(&**value, Expr::Undefined))
    }

    pub(crate) fn index_elements(&mut self, statements: &'h [Stmt]) {
        for pair in statements.array_windows::<2>() {
            if let Some(element) = self.recognize_element(pair) {
                self.elements.insert(element.id, element);
            }
        }
    }

    pub(crate) fn element<'a>(&self, statements: &'a [Stmt]) -> Option<(Element<'h>, &'a [Stmt])> {
        let [Stmt::Let { id, .. }, Stmt::If { condition, .. }, rest @ ..] = statements else {
            return None;
        };
        let element = self.elements.get(id)?;
        std::ptr::eq(condition, element.condition).then_some((*element, rest))
    }

    fn recognize_element(&self, pair: &'h [Stmt; 2]) -> Option<Element<'h>> {
        let [
            Stmt::Let {
                id,
                ty,
                init: Some(Expr::Undefined),
                ..
            },
            Stmt::If {
                condition,
                then_branch,
                else_branch: Some(pull),
            },
        ] = pair
        else {
            return None;
        };
        let Expr::LocalGet(done) = compared_to_true(condition)? else {
            return None;
        };
        let [Stmt::Expr(Expr::LocalSet(target, missing))] = then_branch.as_slice() else {
            return None;
        };
        if target != id || !matches!(&**missing, Expr::Undefined) {
            return None;
        }
        let indexed = pull.iter().find_map(|statement| match statement {
            Stmt::Let {
                init: Some(init), ..
            } if let Expr::Conditional {
                condition,
                else_expr,
                ..
            } = init
                && self.is_guard(condition) =>
            {
                Some(&**else_expr)
            }
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
            ty,
            id: *id,
            value,
            advance,
            done: *done,
            condition,
            missing,
            available,
        })
    }
}

#[derive(Default)]
pub(crate) struct CursorPositions(HashMap<LocalId, usize>);

impl CursorPositions {
    pub(crate) fn reset(&mut self, id: LocalId) {
        self.0.insert(id, 0);
    }
    pub(crate) fn index(&self, expr: &Expr) -> Option<usize> {
        let ("arrayRecordIndex", [Expr::LocalGet(id)]) = runtime(expr)? else {
            return None;
        };
        self.0.get(id).copied()
    }
    pub(crate) fn advance(&mut self, id: LocalId) {
        if let Some(position) = self.0.get_mut(&id) {
            *position += 1;
        }
    }
}
