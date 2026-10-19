//! Statically typed Date construction and UTC mutation.
use super::*;

impl<'s> FunctionBuilder<'s> {
    pub(super) fn date_expression(
        &mut self,
        ctx: &mut Context,
        expr: &'s Expr,
    ) -> Result<Option<Value>> {
        match expr {
            Expr::DateNew(args) => {
                let time = match args.as_slice() {
                    [] => self.intrinsic(ctx, Intrinsic::DateNow, vec![]),
                    [value] => {
                        let value = self.expression(ctx, value)?;
                        match kind(ctx, value).ok() {
                            Some(Kind::String) => {
                                self.intrinsic(ctx, Intrinsic::DateParse, vec![value])
                            }
                            Some(Kind::Date) => {
                                self.intrinsic(ctx, Intrinsic::DateTime, vec![value])
                            }
                            _ => {
                                let value = self.arithmetic(ctx, value);
                                ensure!(
                                    kind(ctx, value)? == Kind::Number,
                                    "Date requires a number, ISO string or Date"
                                );
                                value
                            }
                        }
                    }
                    _ => self.date_utc(ctx, args)?,
                };
                Ok(self.call_intrinsic(ctx, Intrinsic::DateFromTime, vec![time]))
            }
            Expr::DateParse(text) => {
                let text = self.string_operand(ctx, text)?;
                Ok(self.call_intrinsic(ctx, Intrinsic::DateParse, vec![text]))
            }
            Expr::DateUtc(args) => self.date_utc(ctx, args).map(Some),
            _ => Ok(None),
        }
    }

    fn date_utc(&mut self, ctx: &mut Context, args: &'s [Expr]) -> Result<Value> {
        let mut values = Vec::with_capacity(7);
        for (index, default) in [f64::NAN, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]
            .into_iter()
            .enumerate()
        {
            let value = match args.get(index) {
                Some(Expr::Undefined) => self.number(ctx, f64::NAN),
                value => self.number_or(ctx, value, default)?,
            };
            values.push(value);
        }
        for arg in args.iter().skip(7) {
            self.expression(ctx, arg)?;
        }
        Ok(self.intrinsic(ctx, Intrinsic::DateUtc, values))
    }

    pub(super) fn date_set(
        &mut self,
        ctx: &mut Context,
        date: Value,
        part: u32,
        maximum: usize,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let selector = self.number(ctx, part as f64);
        let count = self.number(ctx, args.len().min(maximum) as f64);
        let mut values = vec![date, selector, count];
        for index in 0..4 {
            let value = match args.get(index).filter(|_| index < maximum) {
                None | Some(Expr::Undefined) => self.number(ctx, f64::NAN),
                Some(value) => self.number_operand(ctx, value)?,
            };
            values.push(value);
        }
        for arg in args.iter().skip(maximum) {
            self.expression(ctx, arg)?;
        }
        Ok(self.call_intrinsic(ctx, Intrinsic::DateSet, values))
    }
}
