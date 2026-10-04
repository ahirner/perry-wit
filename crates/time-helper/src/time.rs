//! Allocation-free Gregorian arithmetic and the ISO calendar Temporal subset.
#![forbid(unsafe_code)]

#[path = "format.rs"]
mod format;
#[path = "parse.rs"]
mod parse;

use ixdtf::records::UtcOffsetRecordOrZ;

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;
const INSTANT_LIMIT: i128 = 100_000_000 * NANOS_PER_DAY;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Syntax,
    Range,
    UnsupportedAnnotation,
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
        let date = parsed.date.ok_or(Error::Syntax)?;
        let time = parsed.time.ok_or(Error::Syntax)?;
        let offset = parsed.offset.ok_or(Error::Syntax)?;
        let local = parse::date_time(date, time)?;
        Self::from_epoch_nanoseconds(local.position() - parse::offset_nanoseconds(offset)?)
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
        if parsed.offset == Some(UtcOffsetRecordOrZ::Z) {
            return Err(Error::Syntax);
        }
        if let Some(offset) = parsed.offset {
            parse::offset_nanoseconds(offset)?;
        }
        parse::date_time(
            parsed.date.ok_or(Error::Syntax)?,
            parsed.time.unwrap_or_default(),
        )
    }

    /// The supported Temporal.add({days}) operation, preserving all time fields.
    pub fn add_days(self, days: i64) -> Result<Self, Error> {
        let day = i64::from(self.day).checked_add(days).ok_or(Error::Range)?;
        let day = i32::try_from(day).map_err(|_| Error::Range)?;
        Self { day, ..self }.checked()
    }

    pub fn date(self) -> (i32, u8, u8) {
        datealgo::rd_to_date(self.day)
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

    fn checked(self) -> Result<Self, Error> {
        let limit = INSTANT_LIMIT + NANOS_PER_DAY;
        if self.position() <= -limit || self.position() >= limit {
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
