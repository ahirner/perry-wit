//! Built-in methods whose arguments feed one intrinsic in source order.
use super::super::*;
use Argument::{Number, Padding, SearchString};

pub(super) enum Argument {
    Number(f64),
    Constant(f64),
    SearchString,
    Padding,
    Required(Kind, &'static str),
}

pub(super) enum Return {
    Value,
    Receiver,
    Found,
    Undefined,
}

pub(super) struct IntrinsicCall {
    pub(super) intrinsic: Intrinsic,
    pub(super) arguments: &'static [Argument],
    pub(super) result: Return,
}

impl IntrinsicCall {
    pub(super) fn lower<'s>(
        self,
        builder: &mut FunctionBuilder<'s>,
        ctx: &mut Context,
        receiver: Value,
        args: impl IntoIterator<Item = &'s Expr>,
    ) -> Result<Option<Value>> {
        let mut values = Vec::with_capacity(self.arguments.len() + 1);
        values.push(receiver);
        let mut args = args.into_iter();
        for argument in self.arguments {
            let expr = match argument {
                Argument::Constant(_) => None,
                _ => args.next(),
            };
            values.push(match argument {
                Argument::Constant(value) => builder.number(ctx, *value),
                Number(default) => builder.number_or(ctx, expr, *default)?,
                SearchString => builder.typed_operand(
                    ctx,
                    expr.context("a search string is required")?,
                    Kind::String,
                    "a search string",
                )?,
                Argument::Required(kind, description) => builder.typed_operand(
                    ctx,
                    expr.context("a method argument is required")?,
                    *kind,
                    description,
                )?,
                Padding => match expr {
                    None | Some(Expr::Undefined) => builder.string(ctx, " "),
                    Some(expr) => {
                        let value = builder.expression(ctx, expr)?;
                        builder.display(ctx, value)?
                    }
                },
            });
        }
        for extra in args {
            builder.expression(ctx, extra)?;
        }
        let value = builder.call_intrinsic(ctx, self.intrinsic, values);
        Ok(match self.result {
            Return::Value => value,
            Return::Undefined => {
                let number = FP64Type::get(ctx).into();
                Some(builder.emit_value(ctx, |ctx| UndefinedOp::new(ctx, number)))
            }
            Return::Receiver => Some(receiver),
            Return::Found => {
                let found = value.context("string search must return an index")?;
                let missing = builder.number(ctx, -1.0);
                Some(builder.compare(ctx, CompareKind::Ne, found, missing))
            }
        })
    }
}
