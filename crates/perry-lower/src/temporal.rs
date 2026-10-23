//! ISO Temporal factories, calendar accessors and day/calendar arithmetic.
use super::*;
use perry_abi::layout::time::{DatePart, DurationPart, Overflow, Unit as CalendarUnit};

#[derive(Clone, Copy)]
enum Fields {
    Date,
    DateTime,
    Duration(&'static str),
    Overflow,
    LargestUnit,
}

impl Fields {
    fn names(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Date => &Self::DateTime.names()[..3],
            Self::DateTime => &[
                ("year", "a year number"),
                ("month", "a month number"),
                ("day", "a day number"),
                ("hour", "an hour number"),
                ("minute", "a minute number"),
                ("second", "a second number"),
                ("millisecond", "a millisecond number"),
                ("microsecond", "a microsecond number"),
                ("nanosecond", "a nanosecond number"),
            ],
            Self::Duration(_) => &[
                ("years", "a number of years"),
                ("months", "a number of months"),
                ("weeks", "a number of weeks"),
                ("days", "a number of days"),
            ],
            Self::Overflow => &[("overflow", "an overflow string")],
            Self::LargestUnit => &[("largestUnit", "a largestUnit string")],
        }
    }

    fn choices(self) -> &'static [(u32, &'static [&'static str])] {
        match self {
            Self::Overflow => &[
                (Overflow::Constrain as u32, &["constrain"]),
                (Overflow::Reject as u32, &["reject"]),
            ],
            Self::LargestUnit => &[
                (CalendarUnit::Day as u32, &["day", "days", "auto"]),
                (CalendarUnit::Week as u32, &["week", "weeks"]),
                (CalendarUnit::Month as u32, &["month", "months"]),
                (CalendarUnit::Year as u32, &["year", "years"]),
            ],
            _ => &[],
        }
    }

    fn class(self) -> &'static str {
        match self {
            Self::Date => "PlainDate",
            Self::DateTime => "PlainDateTime",
            _ => "",
        }
    }

    fn object_error(self) -> String {
        match self {
            Self::Date | Self::DateTime => format!(
                "Temporal.{}.from fields object requires year, month, and day",
                self.class()
            ),
            Self::Duration(method) => format!(
                "Temporal {method} takes a duration object {{ years?, months?, weeks?, days? }}"
            ),
            Self::Overflow => "Temporal options must be an object".into(),
            Self::LargestUnit => "Temporal until/since options must be an object or string".into(),
        }
    }
}

#[derive(Clone, Copy)]
enum FieldValue<'s> {
    Literal(&'s Expr),
    Loaded(Value),
}

impl<'s> FunctionBuilder<'s> {
    pub(super) fn raise_range_error(&mut self, ctx: &mut Context, msg: &str) {
        let message = self.string(ctx, msg);
        let error = self.emit_value(ctx, |ctx| {
            ErrorNewOp::new(ctx, ErrorKind::RangeError, message)
        });
        let status = self.emit_value(ctx, |ctx| RaiseOp::new(ctx, error));
        self.propagate(ctx, status);
        self.after_terminator(ctx);
    }

    fn object_literal_pairs(
        &self,
        expr: &'s Expr,
    ) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s>> {
        let (fields, defined, names, args) = match expr {
            Expr::Object(fields) => (&fields[..], &[][..], &[][..], &[][..]),
            // Perry HIR keeps a literal that repeats a key with an effectful value as its
            // source-ordered definitions; later pairs overwrite earlier ones, as in a literal.
            Expr::ObjectSpread { parts } if parts.iter().all(|(key, _)| key.is_some()) => {
                (&[][..], &parts[..], &[][..], &[][..])
            }
            Expr::New {
                class_name, args, ..
            } => (
                &[][..],
                &[][..],
                self.shapes.literal_fields(class_name)?,
                &args[..],
            ),
            _ => return None,
        };
        Some(
            fields
                .iter()
                .map(|(name, value)| (name.as_str(), value))
                .chain(
                    defined
                        .iter()
                        .filter_map(|(name, value)| Some((name.as_deref()?, value))),
                )
                .chain(names.iter().copied().zip(args)),
        )
    }

    fn temporal_fields(
        &mut self,
        ctx: &mut Context,
        input: Option<FieldValue<'s>>,
        schema: Fields,
    ) -> Result<Vec<Option<Value>>> {
        let names = schema.names();
        let mut fields = vec![None; names.len()];
        let mut literal = false;
        let record = match input {
            Some(FieldValue::Literal(expr)) if self.object_literal_pairs(expr).is_some() => {
                literal = true;
                for (name, expr) in self.object_literal_pairs(expr).into_iter().flatten() {
                    let index = names.iter().position(|&(field, _)| field == name);
                    if index.is_none()
                        && let Fields::Duration(method) = schema
                    {
                        bail!(
                            "Temporal {method} duration field {name} is not supported; use years, months, weeks, or days"
                        );
                    }
                    let value = match expr {
                        Expr::String(_) | Expr::Undefined => FieldValue::Literal(expr),
                        _ => FieldValue::Loaded(self.expression(ctx, expr)?),
                    };
                    if let Some(index) = index {
                        fields[index] = Some(value);
                    }
                }
                None
            }
            None => None,
            Some(FieldValue::Literal(Expr::Undefined)) if !schema.choices().is_empty() => None,
            Some(FieldValue::Literal(expr @ Expr::String(_)))
                if matches!(schema, Fields::LargestUnit) =>
            {
                fields[0] = Some(FieldValue::Literal(expr));
                None
            }
            Some(FieldValue::Literal(expr)) => Some(self.expression(ctx, expr)?),
            Some(FieldValue::Loaded(value)) => Some(value),
        };
        if let Some(value) = record {
            let value = if schema.choices().is_empty() {
                value
            } else {
                self.defined(ctx, value)?
            };
            if matches!(schema, Fields::LargestUnit) && kind(ctx, value)? == Kind::String {
                fields[0] = Some(FieldValue::Loaded(value));
            } else {
                let record = TypedHandle::<RecordType>::from_handle(value.get_type(ctx), ctx)
                    .with_context(|| schema.object_error())?;
                for (slot, &(name, _)) in fields.iter_mut().zip(names) {
                    if record.deref(ctx).field(name).is_some() {
                        let op = FieldGetOp::new(ctx, value, name.into())?;
                        *slot = Some(FieldValue::Loaded(self.field_read(ctx, &op)?));
                    }
                }
            }
        }
        let choices = schema.choices();
        let option = !choices.is_empty();
        let mut values = vec![None; names.len()];
        for (index, field) in fields.into_iter().enumerate() {
            let default_time = matches!(schema, Fields::DateTime) && index >= 3;
            let field = field.filter(|field| {
                !(option || default_time) || !matches!(field, FieldValue::Literal(Expr::Undefined))
            });
            let Some(field) = field else {
                if !literal && index < 3 && matches!(schema, Fields::Date | Fields::DateTime) {
                    bail!("{}", schema.object_error());
                }
                values[index] = choices
                    .first()
                    .map(|&(code, _)| self.number(ctx, code as f64));
                continue;
            };
            if let FieldValue::Literal(Expr::String(name)) = field
                && option
            {
                let code = choices
                    .iter()
                    .find(|(_, names)| names.contains(&name.as_str()));
                ensure!(
                    code.is_some() || matches!(schema, Fields::Overflow),
                    "unsupported Temporal largestUnit {name:?}"
                );
                values[index] = Some(self.number(ctx, code.map_or(2.0, |&(code, _)| code as f64)));
                continue;
            }
            let mut value = match field {
                FieldValue::Literal(expr) => self.expression(ctx, expr)?,
                FieldValue::Loaded(value) => value,
            };
            if self.is_optional(ctx, value)
                && (option || default_time || !literal && matches!(schema, Fields::Duration(_)))
            {
                let undefined = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, value));
                let default = if let Some((_, names)) = choices.first() {
                    self.string(
                        ctx,
                        names.first().context("Temporal option has no spelling")?,
                    )
                } else {
                    self.number(ctx, 0.0)
                };
                value = self.choose(
                    ctx,
                    default.get_type(ctx),
                    undefined,
                    |_, _| Ok(default),
                    |b, ctx| b.strip_undefined(ctx, value),
                )?;
            } else if literal {
                value = self.defined(ctx, value)?;
            }
            let (name, what) = names[index];
            ensure!(
                kind(ctx, value)? == if option { Kind::String } else { Kind::Number },
                "{}",
                if literal {
                    format!("{what} is required here")
                } else {
                    match schema {
                        Fields::Date | Fields::DateTime => {
                            format!("Temporal.{}.from {name} must be a number", schema.class())
                        }
                        Fields::Duration(method) => {
                            format!("Temporal {method} {name} field must be a number")
                        }
                        _ => format!("{what} is required here"),
                    }
                }
            );
            let mut selected = if option {
                self.number(ctx, choices.len() as f64)
            } else {
                value
            };
            let ty = selected.get_type(ctx);
            for &(code, names) in choices {
                for name in names {
                    let name = self.string(ctx, name);
                    let matches = self.compare(ctx, CompareKind::Eq, value, name);
                    selected = self.choose(
                        ctx,
                        ty,
                        matches,
                        |b, ctx| Ok(b.number(ctx, code as f64)),
                        |_, _| Ok(selected),
                    )?;
                }
            }
            values[index] = Some(selected);
        }
        Ok(values)
    }

    fn overflow_constrain(
        &mut self,
        ctx: &mut Context,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let value =
            self.temporal_fields(ctx, options.map(FieldValue::Literal), Fields::Overflow)?[0]
                .context("overflow field default missing")?;
        let constrain = self.number(ctx, Overflow::Constrain as u32 as f64);
        let valid = self.compare(ctx, CompareKind::Le, value, constrain);
        let done = self.new_block(ctx, "overflow_valid");
        let failed = self.new_block(ctx, "overflow_invalid");
        self.branch(ctx, valid, done, failed);
        self.switch_to(failed);
        self.raise_range_error(ctx, "Temporal overflow must be constrain or reject");
        self.switch_to(done);
        Ok(value)
    }

    pub(super) fn coerce_plain_date(
        &mut self,
        ctx: &mut Context,
        item: &'s Expr,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        self.coerce_plain(ctx, item, options, Kind::PlainDate)
    }

    pub(super) fn compare_sign(
        &mut self,
        ctx: &mut Context,
        left: Value,
        right: Value,
    ) -> Result<Value> {
        let lt = self.compare(ctx, CompareKind::Lt, left, right);
        let f64_ty = FP64Type::get(ctx).into();
        self.choose(
            ctx,
            f64_ty,
            lt,
            |b, ctx| Ok(b.number(ctx, -1.0)),
            |b, ctx| {
                let gt = b.compare(ctx, CompareKind::Gt, left, right);
                b.choose(
                    ctx,
                    f64_ty,
                    gt,
                    |b, ctx| Ok(b.number(ctx, 1.0)),
                    |b, ctx| Ok(b.number(ctx, 0.0)),
                )
            },
        )
    }

    fn coerce_plain(
        &mut self,
        ctx: &mut Context,
        item: &'s Expr,
        options: Option<&'s Expr>,
        target: Kind,
    ) -> Result<Value> {
        let date_time = target == Kind::PlainDateTime;
        let schema = if date_time {
            Fields::DateTime
        } else {
            Fields::Date
        };
        let class = schema.class();
        let input = if self.object_literal_pairs(item).is_some() {
            FieldValue::Literal(item)
        } else {
            let value = self.expression(ctx, item)?;
            let input_kind = kind(ctx, value)?;
            if input_kind == target || input_kind == Kind::String {
                self.overflow_constrain(ctx, options)?;
                return Ok(if input_kind == target {
                    value
                } else {
                    let intrinsic = if date_time {
                        Intrinsic::PlainDateTimeFrom
                    } else {
                        Intrinsic::PlainDateFrom
                    };
                    self.intrinsic(ctx, intrinsic, vec![value])
                });
            }
            ensure!(
                input_kind == Kind::Record,
                "Temporal.{class}.from expects an ISO string, {class}, or {{ year, month, day }}, got {}",
                input_kind.label()
            );
            FieldValue::Loaded(value)
        };
        let fields = self.temporal_fields(ctx, Some(input), schema)?;
        ensure!(
            fields[..3].iter().all(Option::is_some),
            "{}",
            schema.object_error()
        );
        let zero = self.number(ctx, 0.0);
        let mut values: Vec<_> = fields
            .into_iter()
            .map(|value| value.unwrap_or(zero))
            .collect();
        values.push(self.overflow_constrain(ctx, options)?);
        let intrinsic = if date_time {
            Intrinsic::PlainDateTimeFromFields
        } else {
            Intrinsic::PlainDateFromFields
        };
        Ok(self.intrinsic(ctx, intrinsic, values))
    }

    /// `Temporal.Instant.from(string)`, `Instant.fromEpochMilliseconds(number)`,
    /// `PlainDate.from(item, options?)`, `PlainDate.compare(a, b)`, `PlainDateTime.from(item, options?)` and `Temporal.Now.*`.
    pub(super) fn temporal_factory(
        &mut self,
        ctx: &mut Context,
        class: &str,
        factory: &str,
        args: &'s [Expr],
    ) -> Result<Value> {
        match (class, factory) {
            ("Now", "timeZoneId") => {
                ensure!(
                    args.is_empty(),
                    "Temporal.Now.timeZoneId takes no arguments"
                );
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
                let local_iso = self.intrinsic(ctx, Intrinsic::StringSlice, vec![iso, start, end]);
                return Ok(self.intrinsic(ctx, Intrinsic::PlainDateTimeFrom, vec![local_iso]));
            }
            ("PlainDate" | "PlainDateTime", "from") => {
                ensure!(
                    (1..=2).contains(&args.len()),
                    "Temporal.{class}.from takes one or two arguments"
                );
                let target = if class == "PlainDate" {
                    Kind::PlainDate
                } else {
                    Kind::PlainDateTime
                };
                return self.coerce_plain(ctx, &args[0], args.get(1), target);
            }
            ("PlainDate", "compare") => {
                ensure!(
                    args.len() == 2,
                    "Temporal.PlainDate.compare takes two arguments"
                );
                let a = self.coerce_plain_date(ctx, &args[0], None)?;
                let b = self.coerce_plain_date(ctx, &args[1], None)?;
                let da = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![a]);
                let db = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![b]);
                return self.compare_sign(ctx, da, db);
            }
            _ => {}
        }
        let (intrinsic, input, what) = match (class, factory) {
            ("Instant", "fromEpochMilliseconds") => {
                (Intrinsic::InstantFromMs, Kind::Number, "a number")
            }
            ("Instant", "from") => (Intrinsic::InstantFrom, Kind::String, "an ISO string"),
            _ => bail!(
                "Temporal.{class}.{factory} is not supported; use Instant.from, Instant.fromEpochMilliseconds, PlainDate.from, PlainDate.compare, PlainDateTime.from or Temporal.Now"
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
            "year" => DatePart::Year,
            "month" => DatePart::Month,
            "day" => DatePart::Day,
            "dayOfWeek" => DatePart::Weekday,
            "hour" if !date => DatePart::Hour,
            "minute" if !date => DatePart::Minute,
            "second" if !date => DatePart::Second,
            "millisecond" if !date => DatePart::Millisecond,
            "microsecond" if !date => DatePart::Microsecond,
            "nanosecond" if !date => DatePart::Nanosecond,
            other => bail!(
                "property {other} is not supported on {} yet",
                receiver_kind.label()
            ),
        };
        let part = self.number(ctx, f64::from(part as u32));
        let intrinsic = if date {
            Intrinsic::PlainDatePart
        } else {
            Intrinsic::PlainDateTimePart
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value, part]))
    }

    pub(super) fn temporal_until(
        &mut self,
        ctx: &mut Context,
        start: Value,
        end: Value,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let unit =
            self.temporal_fields(ctx, options.map(FieldValue::Literal), Fields::LargestUnit)?[0]
                .context("largestUnit field default missing")?;
        let da = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![start]);
        let db = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![end]);
        let diff = self.binary(ctx, BinaryKind::Sub, db, da);
        let zero = self.number(ctx, 0.0);
        let blank = self.compare(ctx, CompareKind::Eq, diff, zero);
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        let sign = self.compare_sign(ctx, diff, zero)?;
        let month = self.number(ctx, CalendarUnit::Month as u32 as f64);
        let calendar = self.compare(ctx, CompareKind::Ge, unit, month);
        let week = self.number(ctx, CalendarUnit::Week as u32 as f64);
        let weekly = self.compare(ctx, CompareKind::Eq, unit, week);
        let mut parts = [zero; 4];
        for (index, part) in [
            DurationPart::Years,
            DurationPart::Months,
            DurationPart::Weeks,
            DurationPart::Days,
        ]
        .into_iter()
        .enumerate()
        {
            parts[index] = self.choose(
                ctx,
                f64_ty,
                calendar,
                |b, ctx| {
                    if part == DurationPart::Weeks {
                        return Ok(zero);
                    }
                    let part = b.number(ctx, part as u32 as f64);
                    Ok(b.intrinsic(
                        ctx,
                        Intrinsic::PlainDateUntilPart,
                        vec![start, end, unit, part],
                    ))
                },
                |b, ctx| match part {
                    DurationPart::Years | DurationPart::Months => Ok(zero),
                    DurationPart::Weeks | DurationPart::Days => b.choose(
                        ctx,
                        f64_ty,
                        weekly,
                        |b, ctx| {
                            let seven = b.number(ctx, 7.0);
                            let raw = if part == DurationPart::Weeks {
                                let quotient = b.binary(ctx, BinaryKind::Div, diff, seven);
                                b.intrinsic(ctx, Intrinsic::MathTrunc, vec![quotient])
                            } else {
                                b.binary(ctx, BinaryKind::Rem, diff, seven)
                            };
                            Ok(b.binary(ctx, BinaryKind::Add, raw, zero))
                        },
                        |_, _| {
                            Ok(if part == DurationPart::Weeks {
                                zero
                            } else {
                                diff
                            })
                        },
                    ),
                },
            )?;
        }
        let [years, months, weeks, days] = parts;
        let bool_ty: TypeHandle = IntegerType::get(ctx, 1, Signedness::Signless).into();
        let record = self.shapes.mir_record(
            ctx,
            vec![
                ("blank".into(), bool_ty),
                ("days".into(), f64_ty),
                ("months".into(), f64_ty),
                ("sign".into(), f64_ty),
                ("weeks".into(), f64_ty),
                ("years".into(), f64_ty),
            ],
        )?;
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, record, vec![blank, days, months, sign, weeks, years])
        }))
    }

    pub(super) fn temporal_add(
        &mut self,
        ctx: &mut Context,
        receiver: Value,
        duration: &'s Expr,
        options: Option<&'s Expr>,
        subtract: bool,
    ) -> Result<Option<Value>> {
        let receiver_kind = kind(ctx, receiver)?;
        let method = if subtract { "subtract" } else { "add" };
        let fields = self.temporal_fields(
            ctx,
            Some(FieldValue::Literal(duration)),
            Fields::Duration(method),
        )?;
        let (years, months, weeks, days) = (fields[0], fields[1], fields[2], fields[3]);
        ensure!(
            years.is_some() || months.is_some() || weeks.is_some() || days.is_some(),
            "Temporal {method} requires at least one of years, months, weeks, or days"
        );
        if years.is_none()
            && months.is_none()
            && weeks.is_none()
            && options.is_none()
            && let Some(mut d) = days
        {
            if subtract {
                let zero = self.number(ctx, 0.0);
                d = self.binary(ctx, BinaryKind::Sub, zero, d);
            }
            let intrinsic = if receiver_kind == Kind::PlainDate {
                Intrinsic::PlainDateAddDays
            } else {
                Intrinsic::PlainDateTimeAddDays
            };
            return Ok(self.call_intrinsic(ctx, intrinsic, vec![receiver, d]));
        }
        let zero = self.number(ctx, 0.0);
        let mut negate_if_sub = |v: Option<Value>| -> Value {
            match v {
                Some(val) if subtract => self.binary(ctx, BinaryKind::Sub, zero, val),
                Some(val) => val,
                None => zero,
            }
        };
        let y = negate_if_sub(years);
        let m = negate_if_sub(months);
        let w = negate_if_sub(weeks);
        let d = negate_if_sub(days);
        let constrain = self.overflow_constrain(ctx, options)?;
        let intrinsic = if receiver_kind == Kind::PlainDate {
            Intrinsic::PlainDateAddCalendar
        } else {
            Intrinsic::PlainDateTimeAddCalendar
        };
        Ok(self.call_intrinsic(ctx, intrinsic, vec![receiver, y, m, w, d, constrain]))
    }
}
