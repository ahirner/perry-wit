//! Bounded RegExp constructors use the constant text of their lexical operands.
use crate::engine::types::{IdxVec, LocalIdx};
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::LocalId,
};

#[derive(Default)]
enum Constant<'s> {
    #[default]
    Unknown,
    Initializer(&'s Expr),
    Text(String),
    Regex(&'s Expr),
}

#[derive(Default)]
pub(crate) struct StringConstants<'s>(IdxVec<LocalIdx, Constant<'s>>);

impl<'s> StringConstants<'s> {
    pub(crate) fn bind(&mut self, id: LocalIdx, init: &'s Expr, regex: bool, immutable: bool) {
        if !regex && !immutable {
            return;
        }
        self.0.fill_through(id, Constant::default);
        self.0[id] = if regex {
            Constant::Regex(init)
        } else {
            Constant::Initializer(init)
        };
    }

    pub(crate) fn regex(&self, id: LocalIdx) -> Option<&'s Expr> {
        match self.0.get(id)? {
            Constant::Regex(expr) => Some(expr),
            _ => None,
        }
    }

    pub(crate) fn resolve_operands(&mut self, operands: Vec<&Expr>, ids: &[Option<LocalIdx>]) {
        for operand in operands {
            text(operand, &mut String::new(), &mut |id, out| {
                self.resolve(id, ids, out)
            });
        }
        for slot in self.0.as_mut_slice() {
            if matches!(slot, Constant::Initializer(_)) {
                *slot = Constant::Unknown;
            }
        }
    }

    fn resolve(&mut self, id: LocalId, ids: &[Option<LocalIdx>], out: &mut String) -> Option<()> {
        let id = ids.get(id as usize).copied().flatten()?;
        let slot = self.0.get_mut(id)?;
        if let Constant::Initializer(expr) = *slot {
            *slot = Constant::Unknown;
            let mut value = String::new();
            text(expr, &mut value, &mut |id, out| self.resolve(id, ids, out))?;
            self.0[id] = Constant::Text(value);
        }
        let Constant::Text(value) = &self.0[id] else {
            return None;
        };
        out.push_str(value);
        Some(())
    }

    pub(crate) fn regexp_operand(&self, expr: &Expr, ids: &[Option<LocalIdx>]) -> Option<String> {
        let mut value = String::new();
        if !matches!(expr, Expr::Undefined) {
            text(expr, &mut value, &mut |id, out| {
                let Constant::Text(value) = self.0.get(ids.get(id as usize).copied().flatten()?)?
                else {
                    return None;
                };
                out.push_str(value);
                Some(())
            })?;
        }
        Some(value)
    }
}

fn text(
    expr: &Expr,
    output: &mut String,
    binding: &mut impl FnMut(LocalId, &mut String) -> Option<()>,
) -> Option<()> {
    match expr {
        Expr::String(text) => output.push_str(text),
        Expr::LocalGet(id) => binding(*id, output)?,
        Expr::TemplateStringCoerce(value) => text(value, output, binding)?,
        Expr::Sequence(parts) => text(parts.last()?, output, binding)?,
        Expr::Binary {
            op: BinaryOp::Add,
            left,
            right,
        } => {
            text(left, output, binding)?;
            text(right, output, binding)?;
        }
        _ => return None,
    }
    Some(())
}
