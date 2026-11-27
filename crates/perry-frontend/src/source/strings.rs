//! Bounded RegExp constructors use the constant text of their lexical operands.
use super::constructors::RegExpArguments;
use crate::engine::types::{Idx, LocalIdx};
use crate::hir_compat::Locals;
use perry_hir::ir::{BinaryOp, Expr};
use perry_mir::RegexLiteral;
use pliron::context::Context;

enum Constant<'s> {
    Initializer(&'s Expr),
    Text(String),
}

#[derive(Default)]
pub(crate) struct ConstantStrings<'s> {
    strings: Vec<Option<Constant<'s>>>,
}

impl<'s> ConstantStrings<'s> {
    pub(crate) fn bind(&mut self, local: LocalIdx, init: &'s Expr) {
        let index = local.index();
        if index >= self.strings.len() {
            self.strings.resize_with(index + 1, || None);
        }
        self.strings[index] = Some(Constant::Initializer(init));
    }

    pub(crate) fn regexp(
        &mut self,
        ctx: &Context,
        locals: &Locals,
        args: RegExpArguments<'_>,
    ) -> Option<RegexLiteral> {
        Some(RegexLiteral::intern(
            ctx,
            self.operand(locals, args.pattern)?,
            self.operand(locals, args.flags)?,
        ))
    }

    fn operand(&mut self, locals: &Locals, arg: Option<&Expr>) -> Option<String> {
        let mut value = String::new();
        if let Some(arg) = arg
            && !matches!(arg, Expr::Undefined)
        {
            self.append(locals, arg, &mut value)?;
        }
        Some(value)
    }

    fn append(&mut self, locals: &Locals, expr: &Expr, output: &mut String) -> Option<()> {
        match expr {
            Expr::String(text) => output.push_str(text),
            Expr::LocalGet(id) => {
                let i = locals.index(*id)?.index();
                match self.strings.get_mut(i)?.as_ref()? {
                    Constant::Text(text) => output.push_str(text),
                    Constant::Initializer(init) => {
                        let init = *init;
                        self.strings[i] = None;
                        let mut value = String::new();
                        self.append(locals, init, &mut value)?;
                        output.push_str(&value);
                        self.strings[i] = Some(Constant::Text(value));
                    }
                }
            }
            Expr::TemplateStringCoerce(value) => self.append(locals, value, output)?,
            Expr::Sequence(parts) => self.append(locals, parts.last()?, output)?,
            Expr::Binary {
                op: BinaryOp::Add,
                left,
                right,
            } => {
                self.append(locals, left, output)?;
                self.append(locals, right, output)?;
            }
            _ => return None,
        }
        Some(())
    }
}
