//! ISO Temporal factories, calendar accessors and day arithmetic.
use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `Temporal.Instant.from(string)`, `Instant.fromEpochMilliseconds(number)`,
    /// `PlainDate.from(string)` and `PlainDateTime.from(string)`: the supported ISO factories.
    pub(super) fn temporal_factory(
        &mut self,
        ctx: &mut Context,
        class: &str,
        factory: &str,
        args: &'s [Expr],
    ) -> Result<Value> {
        let (intrinsic, input, what) = match (class, factory) {
            ("Instant", "fromEpochMilliseconds") => {
                (Intrinsic::InstantFromMs, Kind::Number, "a number")
            }
            ("Instant", "from") => (Intrinsic::InstantFrom, Kind::String, "an ISO string"),
            ("PlainDate", "from") => (Intrinsic::PlainDateFrom, Kind::String, "an ISO string"),
            ("PlainDateTime", "from") => {
                (Intrinsic::PlainDateTimeFrom, Kind::String, "an ISO string")
            }
            _ => bail!(
                "Temporal.{class}.{factory} is not supported; use Instant.from, \
                 Instant.fromEpochMilliseconds, PlainDate.from or PlainDateTime.from"
            ),
        };
        let [arg] = args else {
            bail!("Temporal.{class}.{factory} takes one argument; options are not supported");
        };
        let arg = self.typed_operand(ctx, arg, input, what)?;
        Ok(self.intrinsic(ctx, intrinsic, vec![arg]))
    }

    /// A field of a `PlainDate` or `PlainDateTime` (`time-helper` part numbers).
    pub(super) fn temporal_field(
        &mut self,
        ctx: &mut Context,
        value: Value,
        field: &str,
    ) -> Result<Value> {
        let receiver_kind = kind(ctx, value)?;
        let date = receiver_kind == Kind::PlainDate;
        let part = match field {
            "year" => 0,
            "month" => 1,
            "day" => 2,
            "dayOfWeek" => 9,
            "hour" if !date => 3,
            "minute" if !date => 4,
            "second" if !date => 5,
            "millisecond" if !date => 6,
            "microsecond" if !date => 7,
            "nanosecond" if !date => 8,
            other => {
                bail!(
                    "property {other} is not supported on {} yet",
                    receiver_kind.label()
                )
            }
        };
        let part = self.number(ctx, f64::from(part));
        let intrinsic = if date {
            Intrinsic::PlainDatePart
        } else {
            Intrinsic::PlainDateTimePart
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value, part]))
    }

    pub(super) fn temporal_method(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        method: &str,
        args: &'s [Expr],
    ) -> Result<Option<Value>> {
        let receiver_kind = kind(ctx, receiver)?;
        Ok(match (receiver_kind, method) {
            (Kind::Instant | Kind::PlainDate | Kind::PlainDateTime, "toString" | "toJSON") => {
                ensure!(
                    args.is_empty(),
                    "Temporal toString options are not supported yet"
                );
                let intrinsic = match receiver_kind {
                    Kind::Instant => Intrinsic::InstantString,
                    Kind::PlainDate => Intrinsic::PlainDateString,
                    _ => Intrinsic::PlainDateTimeString,
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver])
            }
            (Kind::PlainDate | Kind::PlainDateTime, "add") => {
                let days = match args {
                    [
                        Expr::New {
                            class_name, args, ..
                        },
                    ] if self
                        .shapes
                        .literal_fields(class_name)
                        .is_some_and(|names| names == ["days"]) =>
                    {
                        &args[0]
                    }
                    [Expr::Object(fields)] if fields.len() == 1 && fields[0].0 == "days" => {
                        &fields[0].1
                    }
                    _ => bail!("Temporal add takes a literal {{ days }}"),
                };
                let days = self.typed_operand(ctx, days, Kind::Number, "a number of days")?;
                let intrinsic = if receiver_kind == Kind::PlainDate {
                    Intrinsic::PlainDateAddDays
                } else {
                    Intrinsic::PlainDateTimeAddDays
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, days])
            }
            _ => bail!(
                "Temporal method {method} is not supported on {}",
                receiver_kind.label()
            ),
        })
    }
}
