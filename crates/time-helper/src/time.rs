//! Allocation-free Gregorian arithmetic and the ISO calendar Temporal subset.
#![forbid(unsafe_code)]

#[path = "format.rs"]
mod format;
#[path = "parse.rs"]
pub(crate) mod parse;

use parse::{DateRecord, Offset, TimeRecord};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;
const INSTANT_LIMIT: i128 = 100_000_000 * NANOS_PER_DAY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    Syntax,
    Range,
    UnsupportedAnnotation,
    UnsupportedDateFormat,
    Capacity,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CalendarError {
    InvalidDate,
    OutOfRange,
}

/// An exact instant; no floating-point rounding of sub-millisecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Instant {
    nanoseconds: i128,
}

impl Instant {
    pub(crate) fn from_epoch_nanoseconds(nanoseconds: i128) -> Result<Self, Error> {
        if !(-INSTANT_LIMIT..=INSTANT_LIMIT).contains(&nanoseconds) {
            return Err(Error::Range);
        }
        Ok(Self { nanoseconds })
    }

    pub(crate) fn from_epoch_milliseconds(milliseconds: f64) -> Result<Self, Error> {
        if !milliseconds.is_finite()
            || milliseconds.abs() > 8_640_000_000_000_000.0
            || milliseconds != (milliseconds as i64) as f64
        {
            return Err(Error::Range);
        }
        Self::from_epoch_nanoseconds(i128::from(milliseconds as i64) * 1_000_000)
    }

    /// Temporal.Instant.from with ISO date/time and an explicit numeric offset or Z.
    /// Calendar and named-zone annotations are outside this supported subset.
    pub(crate) fn parse(input: &[u8]) -> Result<Self, Error> {
        let parsed = parse::record(input)?;
        let time = parsed.time.ok_or(Error::Syntax)?;
        let offset = parsed.offset.ok_or(Error::Syntax)?;
        let local = parse::date_time(parsed.date, time)?;
        Self::from_epoch_nanoseconds(
            local.position() - i128::from(parse::offset_nanoseconds(offset)),
        )
    }

    /// UTC interchange: YYYY-MM-DDTHH:mm:ss[.1-9 digits]Z, without leap seconds.
    #[cfg(test)]
    pub(crate) fn parse_utc(input: &[u8]) -> Result<Self, Error> {
        parse::strict_utc(input)?;
        Self::parse(input)
    }

    pub(crate) fn epoch_nanoseconds(self) -> i128 {
        self.nanoseconds
    }

    pub(crate) fn epoch_milliseconds(self) -> i64 {
        self.nanoseconds.div_euclid(1_000_000) as i64
    }

    /// Temporal's default string form, always UTC and with trailing fraction zeros omitted.
    pub(crate) fn format(self, output: &mut [u8]) -> Result<usize, Error> {
        format::date_time(self.to_plain(), true, output)
    }

    /// UTC Date interchange with exactly three millisecond fraction digits.
    pub(crate) fn format_milliseconds(self, output: &mut [u8]) -> Result<usize, Error> {
        if self.nanoseconds % 1_000_000 != 0 {
            return Err(Error::Range);
        }
        format::date_time_precision(self.to_plain(), true, 3, output)
    }

    fn to_plain(self) -> PlainDateTime {
        PlainDateTime {
            day: self.nanoseconds.div_euclid(NANOS_PER_DAY) as i32,
            nanosecond: self.nanoseconds.rem_euclid(NANOS_PER_DAY) as u64,
        }
    }
}

/// ISO calendar fields without a time zone or an implicit conversion to an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PlainDateTime {
    day: i32,
    nanosecond: u64,
}

impl PlainDateTime {
    /// Checked helper storage: ISO day number from 1970-01-01 and nanoseconds within that day.
    pub(crate) fn from_day_and_nanosecond(day: i32, nanosecond: u64) -> Result<Self, Error> {
        if nanosecond >= NANOS_PER_DAY as u64 {
            return Err(Error::Range);
        }
        Self { day, nanosecond }.checked()
    }

    pub(crate) fn day_and_nanosecond(self) -> (i32, u64) {
        (self.day, self.nanosecond)
    }

    /// Temporal.PlainDateTime.from: numeric offsets are ignored; Z is rejected.
    pub(crate) fn parse(input: &[u8]) -> Result<Self, Error> {
        let parsed = parse::record(input)?;
        if parsed.offset == Some(Offset::Z) {
            return Err(Error::Syntax);
        }
        parse::date_time(parsed.date, parsed.time.unwrap_or_default())
    }

    /// Temporal.PlainDate.from: validates like PlainDateTime, then discards the time.
    pub(crate) fn parse_plain_date(input: &[u8]) -> Result<Self, Error> {
        Ok(Self {
            nanosecond: 0,
            ..Self::parse(input)?
        })
    }

    /// Temporal.PlainDate.from({ year, month, day }, { overflow }) construction.
    pub(crate) fn from_fields(
        year: f64,
        month: f64,
        day: f64,
        constrain: bool,
    ) -> Result<Self, Error> {
        parse::date_time(
            date_fields(year, month, day, constrain)?,
            TimeRecord::default(),
        )
    }

    pub(crate) fn from_date_time_fields(fields: [f64; 9], constrain: bool) -> Result<Self, Error> {
        let [year, month, day, rest @ ..] = fields;
        parse::date_time(
            date_fields(year, month, day, constrain)?,
            time_fields(rest, constrain)?,
        )
    }

    /// The supported Temporal.add({days}) operation, preserving all time fields.
    pub(crate) fn add_days(self, days: i64) -> Result<Self, Error> {
        let day = i64::from(self.day).checked_add(days).ok_or(Error::Range)?;
        let day = i32::try_from(day).map_err(|_| Error::Range)?;
        Self { day, ..self }.checked()
    }

    /// Calendar addition for years, months, weeks, and days.
    pub(crate) fn add_calendar(
        self,
        years: i64,
        months: i64,
        weeks: i64,
        days: i64,
        constrain: bool,
    ) -> Result<Self, Error> {
        let mut base = self;
        if years != 0 || months != 0 {
            let (y, m, d) = self.date();
            let total_months = i64::from(y)
                .checked_mul(12)
                .and_then(|ym| ym.checked_add(i64::from(m) - 1))
                .and_then(|ym| ym.checked_add(years.checked_mul(12)?))
                .and_then(|ym| ym.checked_add(months))
                .ok_or(Error::Range)?;
            let year = i32::try_from(total_months.div_euclid(12)).map_err(|_| Error::Range)?;
            let month = (total_months.rem_euclid(12) + 1) as u8;
            let day = if constrain && (datealgo::YEAR_MIN..=datealgo::YEAR_MAX).contains(&year) {
                d.min(datealgo::days_in_month(year, month))
            } else {
                d
            };
            let (hour, minute, second, nanosecond) = self.time();
            base = parse::date_time(
                DateRecord { year, month, day },
                TimeRecord {
                    hour,
                    minute,
                    second,
                    nanosecond,
                },
            )?;
        }
        let total_days = weeks
            .checked_mul(7)
            .and_then(|w| w.checked_add(days))
            .ok_or(Error::Range)?;
        base.add_days(total_days)
    }

    /// Signed `(months, remaining_days)` from `self` until `other` in the ISO 8601 calendar.
    pub(crate) fn until_months_and_days(self, other: Self) -> (i32, i32) {
        let sign = (other.day - self.day).signum();
        if sign == 0 {
            return (0, 0);
        }
        let (y1, m1, _) = self.date();
        let (y2, m2, _) = other.date();
        let mut months = (y2 - y1) * 12 + (i32::from(m2) - i32::from(m1));
        let landing = |m: i32| {
            self.add_calendar(0, i64::from(m), 0, 0, true)
                .ok()
                .map(|date| date.day)
                .filter(|&day| (other.day - day) * sign >= 0)
        };
        let mut day = loop {
            match landing(months) {
                Some(day) => break day,
                None => months -= sign,
            }
        };
        while let Some(next) = landing(months + sign) {
            months += sign;
            day = next;
        }
        (months, other.day - day)
    }

    pub(crate) fn date(self) -> (i32, u8, u8) {
        datealgo::rd_to_date(self.day)
    }

    pub(crate) fn weekday(self) -> u8 {
        datealgo::rd_to_weekday(self.day)
    }

    pub(crate) fn time(self) -> (u8, u8, u8, u32) {
        let seconds = self.nanosecond / NANOS_PER_SECOND as u64;
        (
            (seconds / 3600) as u8,
            (seconds / 60 % 60) as u8,
            (seconds % 60) as u8,
            (self.nanosecond % NANOS_PER_SECOND as u64) as u32,
        )
    }

    pub(crate) fn format(self, output: &mut [u8]) -> Result<usize, Error> {
        format::date_time(self, false, output)
    }

    pub(crate) fn format_plain_date(self, output: &mut [u8]) -> Result<usize, Error> {
        format::plain_date(self, output)
    }

    fn checked(self) -> Result<Self, Error> {
        const DAY_LIMIT: i32 = 100_000_001;
        if self.day < -DAY_LIMIT
            || (self.day == -DAY_LIMIT && self.nanosecond == 0)
            || self.day >= DAY_LIMIT
        {
            return Err(Error::Range);
        }
        Ok(self)
    }

    fn position(self) -> i128 {
        i128::from(self.day) * NANOS_PER_DAY + i128::from(self.nanosecond)
    }
}

fn date_fields(year: f64, month: f64, day: f64, constrain: bool) -> Result<DateRecord, Error> {
    if !year.is_finite()
        || year <= f64::from(datealgo::YEAR_MIN) - 1.0
        || year >= f64::from(datealgo::YEAR_MAX) + 1.0
        || month < 1.0
        || day < 1.0
    {
        return Err(Error::Range);
    }
    let year = year as i32;
    let month = regulate(month, 12, constrain)? as u8;
    let day = regulate(
        day,
        i64::from(datealgo::days_in_month(year, month)),
        constrain,
    )? as u8;
    Ok(DateRecord { year, month, day })
}

fn time_fields(fields: [f64; 6], constrain: bool) -> Result<TimeRecord, Error> {
    let mut parts = [0; 6];
    for ((part, field), maximum) in parts
        .iter_mut()
        .zip(fields)
        .zip([23, 59, 59, 999, 999, 999])
    {
        *part = regulate(field, maximum, constrain)?;
    }
    let [hour, minute, second, millisecond, microsecond, nanosecond] = parts;
    Ok(TimeRecord {
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
        nanosecond: (millisecond * 1_000_000 + microsecond * 1_000 + nanosecond) as u32,
    })
}

fn regulate(field: f64, maximum: i64, constrain: bool) -> Result<u64, Error> {
    if !field.is_finite() {
        return Err(Error::Range);
    }
    let integer = field as i64;
    if constrain {
        Ok(integer.clamp(0, maximum) as u64)
    } else if (0..=maximum).contains(&integer) {
        Ok(integer as u64)
    } else {
        Err(Error::Range)
    }
}

/// The standalone calendar consumer's exact YYYY-MM-DD and years 0001–9999 contract.
#[cfg(test)]
pub(crate) fn shift_iso_date(input: &[u8], days: i32) -> Result<[u8; 10], CalendarError> {
    if input.len() != 10
        || input[4] != b'-'
        || input[7] != b'-'
        || !input
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return Err(CalendarError::InvalidDate);
    }
    let original = PlainDateTime::parse(input).map_err(|_| CalendarError::InvalidDate)?;
    if !(1..=9999).contains(&original.date().0) {
        return Err(CalendarError::InvalidDate);
    }
    let shifted = original
        .add_days(i64::from(days))
        .map_err(|_| CalendarError::OutOfRange)?;
    if !(1..=9999).contains(&shifted.date().0) {
        return Err(CalendarError::OutOfRange);
    }
    let mut output = [0; 10];
    format::date(shifted.date(), &mut output);
    Ok(output)
}
