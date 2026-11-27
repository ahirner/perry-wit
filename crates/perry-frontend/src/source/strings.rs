//! Bounded RegExp constructors use the constant text of their lexical operands.
use super::constructors::RegExpArguments;
use perry_hir::{
    ir::{BinaryOp, Expr},
    types::LocalId,
};
use perry_mir::RegexLiteral;
use pliron::{context::Context, uniqued_any::Uniqued};
use std::collections::HashMap;

enum Constant<'s> {
    Initializer(&'s Expr),
    Text(String),
}

#[derive(Default)]
pub(crate) struct ConstantStrings<'s> {
    strings: HashMap<LocalId, Constant<'s>>,
}

impl<'s> ConstantStrings<'s> {
    pub(crate) fn bind(&mut self, id: LocalId, init: &'s Expr) {
        self.strings.insert(id, Constant::Initializer(init));
    }

    pub(crate) fn regexp(
        &mut self,
        ctx: &Context,
        args: RegExpArguments<'_>,
    ) -> Option<Uniqued<RegexLiteral>> {
        Some(RegexLiteral::intern(
            ctx,
            self.operand(args.pattern)?,
            self.operand(args.flags)?,
        ))
    }

    fn operand(&mut self, arg: Option<&Expr>) -> Option<String> {
        let mut value = String::new();
        if let Some(arg) = arg
            && !matches!(arg, Expr::Undefined)
        {
            self.append(arg, &mut value)?;
        }
        Some(value)
    }

    fn append(&mut self, expr: &Expr, output: &mut String) -> Option<()> {
        match expr {
            Expr::String(text) => output.push_str(text),
            Expr::LocalGet(id) => match *self.strings.get(id)? {
                Constant::Text(ref text) => output.push_str(text),
                Constant::Initializer(init) => {
                    self.strings.remove(id);
                    let mut value = String::new();
                    self.append(init, &mut value)?;
                    output.push_str(&value);
                    self.strings.insert(*id, Constant::Text(value));
                }
            },
            Expr::TemplateStringCoerce(value) => self.append(value, output)?,
            Expr::Sequence(parts) => self.append(parts.last()?, output)?,
            Expr::Binary {
                op: BinaryOp::Add,
                left,
                right,
            } => {
                self.append(left, output)?;
                self.append(right, output)?;
            }
            _ => return None,
        }
        Some(())
    }
}
