//! ISO Temporal factories, calendar accessors and day arithmetic.
use super::*;

impl<'s> FunctionBuilder<'s> {
    /// `Temporal.Instant.from(string)`, `Instant.fromEpochMilliseconds(number)`,
    /// `PlainDate.from(string)`, `PlainDateTime.from(string)` and `Temporal.Now.*`.
    pub(super) fn temporal_factory(
        &mut self,
        ctx: &mut Context,
        class: &str,
        factory: &str,
        args: &'s [Expr],
    ) -> Result<Value> {
        match (class, factory) {
            ("Now", "timeZoneId") => {
                ensure!(args.is_empty(), "Temporal.Now.timeZoneId takes no arguments");
                return Ok(self.string(ctx, "UTC"));
            }
            ("Now", "instant") => {
                ensure!(args.is_empty(), "Temporal.Now.instant takes no arguments");
                let ms = self.intrinsic(ctx, Intrinsic::DateNow, vec![]);
                return Ok(self.intrinsic(ctx, Intrinsic::InstantFromMs, vec![ms]));
            }
            ("Now", "plainDateISO" | "plainDateTimeISO") => {
                if let [arg] = args {
                    if let Expr::String(tz) = arg
                        && !matches!(tz.as_str(), "UTC" | "Z" | "+00:00")
                    {
                        bail!("Temporal.Now.{factory} only supports the UTC time zone");
                    }
                    let _ = self.typed_operand(ctx, arg, Kind::String, "a time zone string")?;
                } else {
                    ensure!(
                        args.is_empty(),
                        "Temporal.Now.{factory} takes at most one time zone argument"
                    );
                }
                let ms = self.intrinsic(ctx, Intrinsic::DateNow, vec![]);
                if factory == "plainDateISO" {
                    let day_ms = self.number(ctx, 86_400_000.0);
                    let quot = self.binary(ctx, BinaryKind::Div, ms, day_ms);
                    let days = self.intrinsic(ctx, Intrinsic::MathFloor, vec![quot]);
                    let epoch = self.string(ctx, "1970-01-01");
                    let base = self.intrinsic(ctx, Intrinsic::PlainDateFrom, vec![epoch]);
                    return Ok(self.intrinsic(ctx, Intrinsic::PlainDateAddDays, vec![base, days]));
                }
                let iso = self.intrinsic(ctx, Intrinsic::DateIso, vec![ms]);
                let start = self.number(ctx, 0.0);
                let end = self.number(ctx, -1.0);
                let local_iso =
                    self.intrinsic(ctx, Intrinsic::StringSlice, vec![iso, start, end]);
                return Ok(self.intrinsic(ctx, Intrinsic::PlainDateTimeFrom, vec![local_iso]));
            }
            _ => {}
        }
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
                 Instant.fromEpochMilliseconds, PlainDate.from, PlainDateTime.from or Temporal.Now"
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
            (Kind::PlainDate | Kind::PlainDateTime, "add" | "subtract") => {
                let mut days = match args {
                    [
                        Expr::New {
                            class_name, args, ..
                        },
                    ] if self
                        .shapes
                        .literal_fields(class_name)
                        .is_some_and(|names| names == ["days"]) =>
                    {
                        self.typed_operand(ctx, &args[0], Kind::Number, "a number of days")?
                    }
                    [Expr::Object(fields)] if fields.len() == 1 && fields[0].0 == "days" => {
                        self.typed_operand(ctx, &fields[0].1, Kind::Number, "a number of days")?
                    }
                    [arg] => {
                        let value = self.expression(ctx, arg)?;
                        let is_days_record = value
                            .get_type(ctx)
                            .deref(ctx)
                            .downcast_ref::<RecordType>()
                            .is_some_and(|record| {
                                record.fields().is_ok_and(|fields| fields.len() == 1)
                            });
                        ensure!(
                            is_days_record,
                            "Temporal {method} takes a literal {{ days }}"
                        );
                        let op = FieldGetOp::new(ctx, value, String::from("days"))
                            .map_err(|_| anyhow!("Temporal {method} takes a literal {{ days }}"))?;
                        let field = self.emit_value(ctx, |_| op);
                        ensure!(
                            kind(ctx, field)? == Kind::Number,
                            "Temporal {method} days field must be a number"
                        );
                        field
                    }
                    _ => bail!("Temporal {method} takes a literal {{ days }}"),
                };
                if method == "subtract" {
                    let zero = self.number(ctx, 0.0);
                    days = self.binary(ctx, BinaryKind::Sub, zero, days);
                }
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
