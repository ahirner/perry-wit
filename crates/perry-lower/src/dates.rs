//! Statically typed Date construction and UTC mutation.
use super::*;

impl FunctionBuilder<'_> {
    pub(super) fn date_expression(
        &mut self,
        ctx: &mut Context,
        expr: &Expr,
    ) -> Result<Option<Value>> {
        let (date, part) = match expr {
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
                return Ok(self.call_intrinsic(ctx, Intrinsic::DateFromTime, vec![time]));
            }
            Expr::DateParse(text) => {
                let text = self.string_operand(ctx, text)?;
                return Ok(self.call_intrinsic(ctx, Intrinsic::DateParse, vec![text]));
            }
            Expr::DateUtc(args) => return self.date_utc(ctx, args).map(Some),
            Expr::DateGetTime(date) | Expr::DateValueOf(date) => {
                let date = self.typed_operand(ctx, date, Kind::Date, "a Date")?;
                return Ok(self.call_intrinsic(ctx, Intrinsic::DateTime, vec![date]));
            }
            Expr::DateToISOString(date) | Expr::DateToJSON(date) => {
                let date = self.typed_operand(ctx, date, Kind::Date, "a Date")?;
                let intrinsic = if matches!(expr, Expr::DateToJSON(_)) {
                    Intrinsic::DateJson
                } else {
                    Intrinsic::DateIso
                };
                return Ok(self.call_intrinsic(ctx, intrinsic, vec![date]));
            }
            Expr::DateGetUtcFullYear(date) | Expr::DateGetFullYear(date) => (date, 0),
            Expr::DateGetUtcMonth(date) | Expr::DateGetMonth(date) => (date, 1),
            Expr::DateGetUtcDate(date) | Expr::DateGetDate(date) => (date, 2),
            Expr::DateGetUtcDay(date) | Expr::DateGetDay(date) => (date, 3),
            Expr::DateGetUtcHours(date) | Expr::DateGetHours(date) => (date, 4),
            Expr::DateGetUtcMinutes(date) | Expr::DateGetMinutes(date) => (date, 5),
            Expr::DateGetUtcSeconds(date) | Expr::DateGetSeconds(date) => (date, 6),
            Expr::DateGetUtcMilliseconds(date) | Expr::DateGetMilliseconds(date) => (date, 7),
            Expr::DateGetTimezoneOffset(date) => (date, 8),
            Expr::DateSetUtcFullYear { date, args } | Expr::DateSetFullYear { date, args } => {
                return self.date_set(ctx, date, 0, args);
            }
            Expr::DateSetUtcMonth { date, args } | Expr::DateSetMonth { date, args } => {
                return self.date_set(ctx, date, 1, args);
            }
            Expr::DateSetUtcDate { date, args } | Expr::DateSetDate { date, args } => {
                return self.date_set(ctx, date, 2, args);
            }
            Expr::DateSetUtcHours { date, args } | Expr::DateSetHours { date, args } => {
                return self.date_set(ctx, date, 3, args);
            }
            Expr::DateSetUtcMinutes { date, args } | Expr::DateSetMinutes { date, args } => {
                return self.date_set(ctx, date, 4, args);
            }
            Expr::DateSetUtcSeconds { date, args } | Expr::DateSetSeconds { date, args } => {
                return self.date_set(ctx, date, 5, args);
            }
            Expr::DateSetUtcMilliseconds { date, args }
            | Expr::DateSetMilliseconds { date, args } => return self.date_set(ctx, date, 6, args),
            Expr::DateSetTime { date, args } => return self.date_set(ctx, date, 7, args),
            _ => return Ok(None),
        };
        let date = self.typed_operand(ctx, date, Kind::Date, "a Date")?;
        let part = self.number(ctx, part as f64);
        Ok(self.call_intrinsic(ctx, Intrinsic::DatePart, vec![date, part]))
    }

    fn date_utc(&mut self, ctx: &mut Context, args: &[Expr]) -> Result<Value> {
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

    fn date_set(
        &mut self,
        ctx: &mut Context,
        date: &Expr,
        part: u32,
        args: &[Expr],
    ) -> Result<Option<Value>> {
        let date = self.typed_operand(ctx, date, Kind::Date, "a Date")?;
        let maximum = [3, 2, 1, 4, 3, 2, 1, 1][part as usize];
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
