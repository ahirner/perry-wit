//! Allocation-free Gregorian arithmetic and the ISO calendar Temporal subset.
#![forbid(unsafe_code)]

#[path = "format.rs"]
mod format;
#[path = "parse.rs"]
mod parse;

use parse::Offset;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;
const INSTANT_LIMIT: i128 = 100_000_000 * NANOS_PER_DAY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Syntax,
    Range,
    UnsupportedAnnotation,
    UnsupportedDateFormat,
    Capacity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalendarError {
    InvalidDate,
    OutOfRange,
}

/// An exact instant; no floating-point rounding of sub-millisecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant {
    nanoseconds: i128,
}

impl Instant {
    pub fn from_epoch_nanoseconds(nanoseconds: i128) -> Result<Self, Error> {
        if !(-INSTANT_LIMIT..=INSTANT_LIMIT).contains(&nanoseconds) {
            return Err(Error::Range);
        }
        Ok(Self { nanoseconds })
    }

    pub fn from_epoch_milliseconds(milliseconds: f64) -> Result<Self, Error> {
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
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        let parsed = parse::record(input)?;
        let time = parsed.time.ok_or(Error::Syntax)?;
        let offset = parsed.offset.ok_or(Error::Syntax)?;
        let local = parse::date_time(parsed.date, time)?;
        Self::from_epoch_nanoseconds(
            local.position() - i128::from(parse::offset_nanoseconds(offset)),
        )
    }

    /// UTC interchange: YYYY-MM-DDTHH:mm:ss[.1-9 digits]Z, without leap seconds.
    pub fn parse_utc(input: &[u8]) -> Result<Self, Error> {
        parse::strict_utc(input)?;
        Self::parse(input)
    }

    pub fn epoch_nanoseconds(self) -> i128 {
        self.nanoseconds
    }

    pub fn epoch_milliseconds(self) -> i64 {
        self.nanoseconds.div_euclid(1_000_000) as i64
    }

    /// Temporal's default string form, always UTC and with trailing fraction zeros omitted.
    pub fn format(self, output: &mut [u8]) -> Result<usize, Error> {
        format::date_time(
            PlainDateTime {
                day: self.nanoseconds.div_euclid(NANOS_PER_DAY) as i32,
                nanosecond: self.nanoseconds.rem_euclid(NANOS_PER_DAY) as u64,
            },
            true,
            output,
        )
    }

    /// UTC Date interchange with exactly three millisecond fraction digits.
    pub fn format_milliseconds(self, output: &mut [u8]) -> Result<usize, Error> {
        if self.nanoseconds % 1_000_000 != 0 {
            return Err(Error::Range);
        }
        format::date_time_precision(
            PlainDateTime {
                day: self.nanoseconds.div_euclid(NANOS_PER_DAY) as i32,
                nanosecond: self.nanoseconds.rem_euclid(NANOS_PER_DAY) as u64,
            },
            true,
            3,
            output,
        )
    }
}

/// ISO calendar fields without a time zone or an implicit conversion to an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlainDateTime {
    day: i32,
    nanosecond: u64,
}

impl PlainDateTime {
    /// Checked helper storage: ISO day number from 1970-01-01 and nanoseconds within that day.
    pub fn from_day_and_nanosecond(day: i32, nanosecond: u64) -> Result<Self, Error> {
        if nanosecond >= NANOS_PER_DAY as u64 {
            return Err(Error::Range);
        }
        Self { day, nanosecond }.checked()
    }

    pub fn day_and_nanosecond(self) -> (i32, u64) {
        (self.day, self.nanosecond)
    }

    /// Temporal.PlainDateTime.from: numeric offsets are ignored; Z is rejected.
    pub fn parse(input: &[u8]) -> Result<Self, Error> {
        let parsed = parse::record(input)?;
        if parsed.offset == Some(Offset::Z) {
            return Err(Error::Syntax);
        }
        parse::date_time(parsed.date, parsed.time.unwrap_or_default())
    }

    /// Temporal.PlainDate.from: validates like PlainDateTime, then discards the time.
    pub fn parse_plain_date(input: &[u8]) -> Result<Self, Error> {
        let value = Self::parse(input)?;
        Ok(Self {
            nanosecond: 0,
            ..value
        })
    }

    /// Temporal.PlainDate.from({ year, month, day }, { overflow }) construction.
    pub fn from_fields(year: f64, month: f64, day: f64, constrain: bool) -> Result<Self, Error> {
        Self::calendar_fields(year, month, day, constrain)?.checked()
    }

    fn calendar_fields(year: f64, month: f64, day: f64, constrain: bool) -> Result<Self, Error> {
        if !year.is_finite() || !month.is_finite() || !day.is_finite() {
            return Err(Error::Range);
        }
        if year <= f64::from(datealgo::YEAR_MIN) - 1.0
            || year >= f64::from(datealgo::YEAR_MAX) + 1.0
            || month < 1.0
            || day < 1.0
        {
            return Err(Error::Range);
        }
        let y = year as i32;
        let month = if constrain {
            if month >= 12.0 { 12 } else { month as u8 }
        } else if month < 13.0 {
            month as u8
        } else {
            return Err(Error::Range);
        };
        let max_day = datealgo::days_in_month(y, month);
        let day = if constrain {
            if day >= f64::from(max_day) {
                max_day
            } else {
                day as u8
            }
        } else if day < f64::from(max_day) + 1.0 {
            day as u8
        } else {
            return Err(Error::Range);
        };
        Ok(Self {
            day: datealgo::date_to_rd((y, month, day)),
            nanosecond: 0,
        })
    }

    pub fn from_date_time_fields(fields: [f64; 9], constrain: bool) -> Result<Self, Error> {
        let [
            year,
            month,
            day,
            hour,
            minute,
            second,
            millisecond,
            microsecond,
            nanosecond,
        ] = fields;
        let mut value = Self::calendar_fields(year, month, day, constrain)?;
        let mut parts = [0; 6];
        for ((part, field), maximum) in parts
            .iter_mut()
            .zip([hour, minute, second, millisecond, microsecond, nanosecond])
            .zip([23, 59, 59, 999, 999, 999])
        {
            if !field.is_finite() {
                return Err(Error::Range);
            }
            let integer = field as i64;
            *part = if constrain {
                integer.clamp(0, maximum)
            } else if (0..=maximum).contains(&integer) {
                integer
            } else {
                return Err(Error::Range);
            } as u64;
        }
        let [hour, minute, second, millisecond, microsecond, nanosecond] = parts;
        value.nanosecond = ((hour * 60 + minute) * 60 + second) * 1_000_000_000
            + millisecond * 1_000_000
            + microsecond * 1_000
            + nanosecond;
        value.checked()
    }

    /// The supported Temporal.add({days}) operation, preserving all time fields.
    pub fn add_days(self, days: i64) -> Result<Self, Error> {
        let day = i64::from(self.day).checked_add(days).ok_or(Error::Range)?;
        let day = i32::try_from(day).map_err(|_| Error::Range)?;
        Self { day, ..self }.checked()
    }

    /// Calendar addition for years, months, weeks, and days.
    pub fn add_calendar(
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
            let new_y = i32::try_from(total_months.div_euclid(12)).map_err(|_| Error::Range)?;
            let new_m = (total_months.rem_euclid(12) + 1) as u8;
            if !(datealgo::YEAR_MIN..=datealgo::YEAR_MAX).contains(&new_y) {
                return Err(Error::Range);
            }
            let max_d = datealgo::days_in_month(new_y, new_m);
            let new_d = if d > max_d {
                if constrain {
                    max_d
                } else {
                    return Err(Error::Range);
                }
            } else {
                d
            };
            base = Self {
                day: datealgo::date_to_rd((new_y, new_m, new_d)),
                nanosecond: self.nanosecond,
            }
            .checked()?;
        }
        let total_days = weeks
            .checked_mul(7)
            .and_then(|w| w.checked_add(days))
            .ok_or(Error::Range)?;
        base.add_days(total_days)
    }

    /// Signed `(months, remaining_days)` from `self` until `other` in the ISO 8601 calendar.
    pub fn until_months_and_days(self, other: Self) -> (i32, i32) {
        let sign = (other.day - self.day).signum();
        if sign == 0 {
            return (0, 0);
        }
        let (y1, m1, _) = self.date();
        let (y2, m2, _) = other.date();
        let mut months = (y2 - y1) * 12 + (i32::from(m2) - i32::from(m1));
        // The day `m` months on, unless it passes `other`. Both ends are representable, so an
        // advance beyond the representable range has passed `other` too.
        let landing = |m: i32| {
            self.add_calendar(0, i64::from(m), 0, 0, true)
                .ok()
                .map(|date| date.day)
                .filter(|&day| (other.day - day) * sign >= 0)
        };
        // Zero months lands on `self`, which never passes `other`.
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

    pub fn date(self) -> (i32, u8, u8) {
        datealgo::rd_to_date(self.day)
    }

    pub fn weekday(self) -> u8 {
        datealgo::rd_to_weekday(self.day)
    }

    pub fn time(self) -> (u8, u8, u8, u32) {
        let seconds = self.nanosecond / NANOS_PER_SECOND as u64;
        (
            (seconds / 3600) as u8,
            (seconds / 60 % 60) as u8,
            (seconds % 60) as u8,
            (self.nanosecond % NANOS_PER_SECOND as u64) as u32,
        )
    }

    pub fn format(self, output: &mut [u8]) -> Result<usize, Error> {
        format::date_time(self, false, output)
    }

    pub fn format_plain_date(self, output: &mut [u8]) -> Result<usize, Error> {
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

/// The standalone calendar consumer's exact YYYY-MM-DD and years 0001–9999 contract.
pub fn shift_iso_date(input: &[u8], days: i32) -> Result<[u8; 10], CalendarError> {
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
