//! ISO Temporal factories, calendar accessors and day/calendar arithmetic.
use super::*;
use perry_abi::layout::time::{DatePart, DurationPart, Overflow, Unit as CalendarUnit};

/// The `largestUnit` of `until`/`since`: a literal decided while lowering, or a runtime string.
enum Unit<'s> {
    Static(&'s str),
    Dynamic(Value),
}

impl<'s> FunctionBuilder<'s> {
    fn numeric_fields<const N: usize>(
        &mut self,
        ctx: &mut Context,
        pairs: impl IntoIterator<Item = (&'s str, &'s Expr)>,
        fields: [(&str, &str); N],
        unknown: impl Fn(&str) -> Result<()>,
    ) -> Result<[Option<Value>; N]> {
        let mut values = [None; N];
        for (name, expr) in pairs {
            if let Some(index) = fields.iter().position(|&(field, _)| field == name) {
                values[index] =
                    Some(self.typed_operand(ctx, expr, Kind::Number, fields[index].1)?);
            } else {
                unknown(name)?;
            }
        }
        Ok(values)
    }

    fn object_literal_pairs(
        &self,
        expr: &'s Expr,
    ) -> Option<impl Iterator<Item = (&'s str, &'s Expr)> + use<'s>> {
        let (fields, names, args) = match expr {
            Expr::Object(fields) => (&fields[..], &[][..], &[][..]),
            Expr::New {
                class_name, args, ..
            } => (&[][..], self.shapes.literal_fields(class_name)?, &args[..]),
            _ => return None,
        };
        Some(
            fields
                .iter()
                .map(|(name, value)| (name.as_str(), value))
                .chain(names.iter().copied().zip(args)),
        )
    }

    fn overflow_string_to_constrain(&mut self, ctx: &mut Context, expr: &'s Expr) -> Result<Value> {
        if let Expr::String(s) = expr {
            return Ok(match s.as_str() {
                "constrain" => self.number(ctx, Overflow::Constrain as u32 as f64),
                "reject" => self.number(ctx, Overflow::Reject as u32 as f64),
                _ => self.number(ctx, 2.0),
            });
        }
        let val = self.typed_operand(ctx, expr, Kind::String, "an overflow string")?;
        self.overflow_from_value(ctx, val)
    }

    fn overflow_from_value(&mut self, ctx: &mut Context, val: Value) -> Result<Value> {
        let constrain_str = self.string(ctx, "constrain");
        let is_constrain = self.compare(ctx, CompareKind::Eq, val, constrain_str);
        let f64_ty = FP64Type::get(ctx).into();
        self.choose(
            ctx,
            f64_ty,
            is_constrain,
            |b, ctx| Ok(b.number(ctx, 1.0)),
            |b, ctx| {
                let reject_str = b.string(ctx, "reject");
                let is_reject = b.compare(ctx, CompareKind::Eq, val, reject_str);
                b.choose(
                    ctx,
                    f64_ty,
                    is_reject,
                    |b, ctx| Ok(b.number(ctx, 0.0)),
                    |b, ctx| Ok(b.number(ctx, 2.0)),
                )
            },
        )
    }

    fn overflow_constrain(
        &mut self,
        ctx: &mut Context,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let Some(opt) = options else {
            return Ok(self.number(ctx, 1.0));
        };
        if matches!(opt, Expr::Undefined) {
            return Ok(self.number(ctx, 1.0));
        }
        if let Some(pairs) = self.object_literal_pairs(opt) {
            let Some((_, ov_expr)) = pairs.into_iter().find(|(k, _)| *k == "overflow") else {
                return Ok(self.number(ctx, 1.0));
            };
            if matches!(ov_expr, Expr::Undefined) {
                return Ok(self.number(ctx, 1.0));
            }
            return self.overflow_string_to_constrain(ctx, ov_expr);
        }
        let opt_val = self.expression(ctx, opt)?;
        let opt_val = self.defined(ctx, opt_val)?;
        let rec = TypedHandle::<RecordType>::from_handle(opt_val.get_type(ctx), ctx)
            .context("Temporal options must be an object")?;
        if rec.deref(ctx).field("overflow").is_none() {
            return Ok(self.number(ctx, 1.0));
        }
        let op = FieldGetOp::new(ctx, opt_val, String::from("overflow"))?;
        let raw_ov = self.field_read(ctx, &op)?;
        let ov_val = if self.is_optional(ctx, raw_ov) {
            let is_undef = self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, raw_ov));
            let str_ty = StringType::get(ctx).into();
            self.choose(
                ctx,
                str_ty,
                is_undef,
                |b, ctx| Ok(b.string(ctx, "constrain")),
                |b, ctx| b.strip_undefined(ctx, raw_ov),
            )?
        } else {
            raw_ov
        };
        self.overflow_from_value(ctx, ov_val)
    }

    fn coerce_plain_date(
        &mut self,
        ctx: &mut Context,
        item: &'s Expr,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        if let Some(pairs) = self.object_literal_pairs(item) {
            let [Some(year), Some(month), Some(day)] = self.numeric_fields(
                ctx,
                pairs,
                [
                    ("year", "a year number"),
                    ("month", "a month number"),
                    ("day", "a day number"),
                ],
                |_| Ok(()),
            )?
            else {
                bail!("Temporal.PlainDate.from fields object requires year, month, and day");
            };
            let constrain = self.overflow_constrain(ctx, options)?;
            return Ok(self.intrinsic(
                ctx,
                Intrinsic::PlainDateFromFields,
                vec![year, month, day, constrain],
            ));
        }
        let val = self.expression(ctx, item)?;
        match kind(ctx, val)? {
            Kind::PlainDate => {
                if options.is_some() {
                    let _ = self.overflow_constrain(ctx, options)?;
                }
                Ok(val)
            }
            Kind::String => {
                if options.is_some() {
                    let _ = self.overflow_constrain(ctx, options)?;
                }
                Ok(self.intrinsic(ctx, Intrinsic::PlainDateFrom, vec![val]))
            }
            Kind::Record => {
                let mut read_num = |field: &str| -> Result<Value> {
                    let op = FieldGetOp::new(ctx, val, String::from(field)).map_err(|_| {
                        anyhow!(
                            "Temporal.PlainDate.from fields object requires year, month, and day"
                        )
                    })?;
                    let v = self.field_read(ctx, &op)?;
                    ensure!(
                        kind(ctx, v)? == Kind::Number,
                        "Temporal.PlainDate.from {field} must be a number"
                    );
                    Ok(v)
                };
                let year = read_num("year")?;
                let month = read_num("month")?;
                let day = read_num("day")?;
                let constrain = self.overflow_constrain(ctx, options)?;
                Ok(self.intrinsic(
                    ctx,
                    Intrinsic::PlainDateFromFields,
                    vec![year, month, day, constrain],
                ))
            }
            other => bail!(
                "Temporal.PlainDate.from expects an ISO string, PlainDate, or {{ year, month, day }}, got {}",
                other.label()
            ),
        }
    }

    /// `Temporal.Instant.from(string)`, `Instant.fromEpochMilliseconds(number)`,
    /// `PlainDate.from(item, options?)`, `PlainDate.compare(a, b)`, `PlainDateTime.from(string)` and `Temporal.Now.*`.
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
            ("PlainDate", "from") => {
                ensure!(
                    (1..=2).contains(&args.len()),
                    "Temporal.PlainDate.from takes one or two arguments"
                );
                return self.coerce_plain_date(ctx, &args[0], args.get(1));
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
                let lt = self.compare(ctx, CompareKind::Lt, da, db);
                let f64_ty = FP64Type::get(ctx).into();
                return self.choose(
                    ctx,
                    f64_ty,
                    lt,
                    |b, ctx| Ok(b.number(ctx, -1.0)),
                    |b, ctx| {
                        let gt = b.compare(ctx, CompareKind::Gt, da, db);
                        b.choose(
                            ctx,
                            f64_ty,
                            gt,
                            |b, ctx| Ok(b.number(ctx, 1.0)),
                            |b, ctx| Ok(b.number(ctx, 0.0)),
                        )
                    },
                );
            }
            _ => {}
        }
        let (intrinsic, input, what) = match (class, factory) {
            ("Instant", "fromEpochMilliseconds") => {
                (Intrinsic::InstantFromMs, Kind::Number, "a number")
            }
            ("Instant", "from") => (Intrinsic::InstantFrom, Kind::String, "an ISO string"),
            ("PlainDateTime", "from") => {
                (Intrinsic::PlainDateTimeFrom, Kind::String, "an ISO string")
            }
            _ => bail!(
                "Temporal.{class}.{factory} is not supported; use Instant.from, \
                 Instant.fromEpochMilliseconds, PlainDate.from, PlainDate.compare, PlainDateTime.from or Temporal.Now"
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
            other => {
                bail!(
                    "property {other} is not supported on {} yet",
                    receiver_kind.label()
                )
            }
        };
        let part = self.number(ctx, f64::from(part as u32));
        let intrinsic = if date {
            Intrinsic::PlainDatePart
        } else {
            Intrinsic::PlainDateTimePart
        };
        Ok(self.intrinsic(ctx, intrinsic, vec![value, part]))
    }

    fn temporal_until(
        &mut self,
        ctx: &mut Context,
        start: Value,
        end: Value,
        options: Option<&'s Expr>,
    ) -> Result<Value> {
        let mut unit = Unit::Static("day");
        if let Some(opt) = options
            && !matches!(opt, Expr::Undefined)
        {
            if let Expr::String(s) = opt {
                unit = Unit::Static(s.as_str());
            } else if let Some(pairs) = self.object_literal_pairs(opt) {
                if let Some((_, lu_expr)) = pairs.into_iter().find(|(k, _)| *k == "largestUnit") {
                    match lu_expr {
                        Expr::Undefined => {}
                        Expr::String(s) => unit = Unit::Static(s.as_str()),
                        other => {
                            unit = Unit::Dynamic(self.typed_operand(
                                ctx,
                                other,
                                Kind::String,
                                "a largestUnit string",
                            )?);
                        }
                    }
                }
            } else {
                let opt_val = self.expression(ctx, opt)?;
                let opt_val = self.defined(ctx, opt_val)?;
                if kind(ctx, opt_val)? == Kind::String {
                    unit = Unit::Dynamic(opt_val);
                } else {
                    let rec = TypedHandle::<RecordType>::from_handle(opt_val.get_type(ctx), ctx)
                        .context("Temporal until/since options must be an object or string")?;
                    if rec.deref(ctx).field("largestUnit").is_some() {
                        let op = FieldGetOp::new(ctx, opt_val, String::from("largestUnit"))?;
                        let raw_lu = self.field_read(ctx, &op)?;
                        unit = Unit::Dynamic(if self.is_optional(ctx, raw_lu) {
                            let is_undef =
                                self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, raw_lu));
                            let str_ty = StringType::get(ctx).into();
                            self.choose(
                                ctx,
                                str_ty,
                                is_undef,
                                |b, ctx| Ok(b.string(ctx, "day")),
                                |b, ctx| b.strip_undefined(ctx, raw_lu),
                            )?
                        } else {
                            raw_lu
                        });
                    }
                }
            }
        }
        let da = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![start]);
        let db = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![end]);
        let diff = self.binary(ctx, BinaryKind::Sub, db, da);
        let zero = self.number(ctx, 0.0);
        let blank = self.compare(ctx, CompareKind::Eq, diff, zero);
        let lt = self.compare(ctx, CompareKind::Lt, diff, zero);
        let f64_ty: TypeHandle = FP64Type::get(ctx).into();
        let sign = self.choose(
            ctx,
            f64_ty,
            lt,
            |b, ctx| Ok(b.number(ctx, -1.0)),
            |b, ctx| {
                let gt = b.compare(ctx, CompareKind::Gt, diff, zero);
                b.choose(
                    ctx,
                    f64_ty,
                    gt,
                    |b, ctx| Ok(b.number(ctx, 1.0)),
                    |b, ctx| Ok(b.number(ctx, 0.0)),
                )
            },
        )?;
        let unit = self.calendar_unit(ctx, unit)?;
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
                (String::from("blank"), bool_ty),
                (String::from("days"), f64_ty),
                (String::from("months"), f64_ty),
                (String::from("sign"), f64_ty),
                (String::from("weeks"), f64_ty),
                (String::from("years"), f64_ty),
            ],
        )?;
        Ok(self.emit_value(ctx, |ctx| {
            AllocOp::new(ctx, record, vec![blank, days, months, sign, weeks, years])
        }))
    }

    fn calendar_unit(&mut self, ctx: &mut Context, unit: Unit<'_>) -> Result<Value> {
        const UNITS: [(CalendarUnit, &[&str]); 4] = [
            (CalendarUnit::Day, &["day", "days", "auto"]),
            (CalendarUnit::Week, &["week", "weeks"]),
            (CalendarUnit::Month, &["month", "months"]),
            (CalendarUnit::Year, &["year", "years"]),
        ];
        let value = match unit {
            Unit::Static(name) => {
                let (unit, _) = UNITS
                    .iter()
                    .find(|(_, names)| names.contains(&name))
                    .with_context(|| format!("unsupported Temporal largestUnit {name:?}"))?;
                return Ok(self.number(ctx, *unit as u32 as f64));
            }
            Unit::Dynamic(value) => value,
        };
        // Invalid codes reach the checked calendar codec and raise RangeError.
        let mut selected = self.number(ctx, 4.0);
        let ty = FP64Type::get(ctx).into();
        for (unit, names) in UNITS {
            for name in names {
                let name = self.string(ctx, name);
                let matches = self.compare(ctx, CompareKind::Eq, value, name);
                selected = self.choose(
                    ctx,
                    ty,
                    matches,
                    |b, ctx| Ok(b.number(ctx, unit as u32 as f64)),
                    |_, _| Ok(selected),
                )?;
            }
        }
        Ok(selected)
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
            (Kind::PlainDate, "equals") => {
                ensure!(
                    args.len() == 1,
                    "Temporal.PlainDate.equals takes one argument"
                );
                let other = self.coerce_plain_date(ctx, &args[0], None)?;
                let da = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![receiver]);
                let db = self.intrinsic(ctx, Intrinsic::PlainDateEpochDays, vec![other]);
                Some(self.compare(ctx, CompareKind::Eq, da, db))
            }
            (Kind::PlainDate, "until" | "since") => {
                ensure!(
                    (1..=2).contains(&args.len()),
                    "Temporal.PlainDate.{method} takes one or two arguments"
                );
                let other = self.coerce_plain_date(ctx, &args[0], None)?;
                let (start, end) = if method == "until" {
                    (receiver, other)
                } else {
                    (other, receiver)
                };
                Some(self.temporal_until(ctx, start, end, args.get(1))?)
            }
            (Kind::PlainDate | Kind::PlainDateTime, "add" | "subtract") => {
                ensure!(
                    (1..=2).contains(&args.len()),
                    "Temporal {method} takes a duration object and optional options"
                );
                let [years, months, weeks, days] = if let Some(pairs) =
                    self.object_literal_pairs(&args[0])
                {
                    self.numeric_fields(
                        ctx,
                        pairs,
                        [
                            ("years", "a number of years"),
                            ("months", "a number of months"),
                            ("weeks", "a number of weeks"),
                            ("days", "a number of days"),
                        ],
                        |other| bail!(
                            "Temporal {method} duration field {other} is not supported; use years, months, weeks, or days"
                        ),
                    )?
                } else {
                    let value = self.expression(ctx, &args[0])?;
                    let rec = TypedHandle::<RecordType>::from_handle(value.get_type(ctx), ctx)
                        .map_err(|_| {
                            anyhow!(
                                "Temporal {method} takes a duration object {{ years?, months?, weeks?, days? }}"
                            )
                        })?;
                    let mut read_unit = |name: &str| -> Result<Option<Value>> {
                        if rec.deref(ctx).field(name).is_none() {
                            return Ok(None);
                        }
                        let op = FieldGetOp::new(ctx, value, String::from(name))?;
                        let field = self.field_read(ctx, &op)?;
                        let field = if self.is_optional(ctx, field) {
                            let is_undef =
                                self.emit_value(ctx, |ctx| IsUndefinedOp::new(ctx, field));
                            let f64_ty = FP64Type::get(ctx).into();
                            self.choose(
                                ctx,
                                f64_ty,
                                is_undef,
                                |b, ctx| Ok(b.number(ctx, 0.0)),
                                |b, ctx| b.strip_undefined(ctx, field),
                            )?
                        } else {
                            field
                        };
                        ensure!(
                            kind(ctx, field)? == Kind::Number,
                            "Temporal {method} {name} field must be a number"
                        );
                        Ok(Some(field))
                    };
                    [
                        read_unit("years")?,
                        read_unit("months")?,
                        read_unit("weeks")?,
                        read_unit("days")?,
                    ]
                };
                ensure!(
                    years.is_some() || months.is_some() || weeks.is_some() || days.is_some(),
                    "Temporal {method} requires at least one of years, months, weeks, or days"
                );
                if years.is_none()
                    && months.is_none()
                    && weeks.is_none()
                    && args.len() == 1
                    && let Some(mut d) = days
                {
                    if method == "subtract" {
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
                        Some(val) if method == "subtract" => {
                            self.binary(ctx, BinaryKind::Sub, zero, val)
                        }
                        Some(val) => val,
                        None => zero,
                    }
                };
                let y = negate_if_sub(years);
                let m = negate_if_sub(months);
                let w = negate_if_sub(weeks);
                let d = negate_if_sub(days);
                let constrain = self.overflow_constrain(ctx, args.get(1))?;
                let intrinsic = if receiver_kind == Kind::PlainDate {
                    Intrinsic::PlainDateAddCalendar
                } else {
                    Intrinsic::PlainDateTimeAddCalendar
                };
                self.call_intrinsic(ctx, intrinsic, vec![receiver, y, m, w, d, constrain])
            }
            _ => bail!(
                "Temporal method {method} is not supported on {}",
                receiver_kind.label()
            ),
        })
    }
}
