//! Bounded RegExp constructors use the constant text of their lexical operands.
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::LocalId,
};
use std::collections::HashMap;

#[derive(Default)]
pub(crate) struct StringConstants(HashMap<LocalId, String>);

impl StringConstants {
    pub(crate) fn for_operands(
        mut bindings: HashMap<LocalId, &Expr>,
        operands: Vec<&Expr>,
    ) -> Self {
        let mut strings = Self::default();
        for operand in operands {
            text(operand, &mut String::new(), &mut |id, output| {
                strings.resolve(id, &mut bindings, output)
            });
        }
        strings
    }

    fn resolve(
        &mut self,
        id: LocalId,
        bindings: &mut HashMap<LocalId, &Expr>,
        output: &mut String,
    ) -> Option<()> {
        if let Some(init) = bindings.remove(&id) {
            let mut value = String::new();
            text(init, &mut value, &mut |id, output| {
                self.resolve(id, bindings, output)
            })?;
            self.0.insert(id, value);
        }
        output.push_str(self.0.get(&id)?);
        Some(())
    }

    pub(crate) fn regexp_operand(&self, expr: &Expr) -> Option<String> {
        let mut value = String::new();
        if !matches!(expr, Expr::Undefined) {
            text(expr, &mut value, &mut |id, output| {
                output.push_str(self.0.get(&id)?);
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
