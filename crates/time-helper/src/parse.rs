use super::{Error, NANOS_PER_SECOND, PlainDateTime};

#[derive(Clone, Copy)]
pub(crate) struct DateRecord {
    pub(crate) year: i32,
    pub(crate) month: u8,
    pub(crate) day: u8,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TimeRecord {
    pub(crate) hour: u8,
    pub(crate) minute: u8,
    pub(crate) second: u8,
    pub(crate) nanosecond: u32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offset {
    Z,
    Nanoseconds(i64),
}

pub(crate) struct ParsedRecord {
    pub(crate) date: DateRecord,
    pub(crate) time: Option<TimeRecord>,
    pub(crate) offset: Option<Offset>,
}

pub(crate) struct Cursor<'a> {
    pub(crate) bytes: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Cursor<'a> {
    pub(crate) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    pub(crate) fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(crate) fn digits(&mut self, count: usize) -> Result<u32, Error> {
        let end = self.pos.checked_add(count).ok_or(Error::Syntax)?;
        let slice = self.bytes.get(self.pos..end).ok_or(Error::Syntax)?;
        let mut value = 0u32;
        for &byte in slice {
            if !byte.is_ascii_digit() {
                return Err(Error::Syntax);
            }
            value = value * 10 + u32::from(byte - b'0');
        }
        self.pos = end;
        Ok(value)
    }

    pub(crate) fn year(&mut self) -> Result<i32, Error> {
        match self.peek() {
            Some(sign @ (b'+' | b'-')) => {
                self.pos += 1;
                let raw = self.digits(6)? as i32;
                if sign == b'-' {
                    if raw == 0 {
                        return Err(Error::Syntax);
                    }
                    Ok(-raw)
                } else {
                    Ok(raw)
                }
            }
            _ => Ok(self.digits(4)? as i32),
        }
    }

    fn fraction_nanos(&mut self) -> Result<u32, Error> {
        let start = self.pos;
        let mut value = 0u32;
        let mut scale = 100_000_000u32;
        while let Some(byte) = self.peek() {
            if !byte.is_ascii_digit() {
                break;
            }
            if scale == 0 {
                return Err(Error::Syntax);
            }
            value += u32::from(byte - b'0') * scale;
            scale /= 10;
            self.pos += 1;
        }
        if self.pos == start {
            return Err(Error::Syntax);
        }
        Ok(value)
    }

    /// Basic and colon-separated hours/minutes/seconds use the same grammar in offsets.
    fn hms(&mut self) -> Result<(u8, u8, Option<u8>), Error> {
        let hour = self.digits(2)? as u8;
        let extended = self.eat(b':');
        if !extended && !self.peek().is_some_and(|b| b.is_ascii_digit()) {
            return Ok((hour, 0, None));
        }
        let minute = self.digits(2)? as u8;
        let seconds = if extended {
            self.eat(b':')
        } else {
            self.peek().is_some_and(|b| b.is_ascii_digit())
        };
        let second = seconds
            .then(|| self.digits(2).map(|n| n as u8))
            .transpose()?;
        Ok((hour, minute, second))
    }
}

pub(crate) fn record(input: &[u8]) -> Result<ParsedRecord, Error> {
    if input.contains(&b'[') {
        return Err(Error::UnsupportedAnnotation);
    }
    let mut cur = Cursor::new(input);
    let year = cur.year()?;
    let (month, day) = if cur.eat(b'-') {
        let month = cur.digits(2)? as u8;
        if !cur.eat(b'-') {
            return Err(Error::Syntax);
        }
        (month, cur.digits(2)? as u8)
    } else {
        (cur.digits(2)? as u8, cur.digits(2)? as u8)
    };
    let date = DateRecord { year, month, day };
    if cur.pos == input.len() {
        return Ok(ParsedRecord {
            date,
            time: None,
            offset: None,
        });
    }
    match cur.peek() {
        Some(b'T' | b't' | b' ') => cur.pos += 1,
        _ => return Err(Error::Syntax),
    }
    let (hour, minute, second) = cur.hms()?;
    let nanosecond = if cur.eat(b'.') || cur.eat(b',') {
        cur.fraction_nanos()?
    } else {
        0
    };
    let time = Some(TimeRecord {
        hour,
        minute,
        second: second.unwrap_or(0),
        nanosecond,
    });
    let offset = match cur.peek() {
        Some(b'Z' | b'z') => {
            cur.pos += 1;
            Some(Offset::Z)
        }
        Some(sign @ (b'+' | b'-')) => {
            cur.pos += 1;
            let (off_hour, off_min, off_sec) = cur.hms()?;
            let off_nano = if off_sec.is_some() && (cur.eat(b'.') || cur.eat(b',')) {
                cur.fraction_nanos()?
            } else {
                0
            };
            let off_sec = off_sec.unwrap_or(0);
            if off_hour > 23 || off_min > 59 || off_sec > 59 {
                return Err(Error::Range);
            }
            let seconds = i64::from(off_hour) * 3600 + i64::from(off_min) * 60 + i64::from(off_sec);
            let total = seconds * (NANOS_PER_SECOND as i64) + i64::from(off_nano);
            Some(Offset::Nanoseconds(if sign == b'-' {
                -total
            } else {
                total
            }))
        }
        _ => None,
    };
    if cur.pos != input.len() {
        return Err(Error::Syntax);
    }
    Ok(ParsedRecord { date, time, offset })
}

pub(crate) fn date_time(date: DateRecord, time: TimeRecord) -> Result<PlainDateTime, Error> {
    if !(datealgo::YEAR_MIN..=datealgo::YEAR_MAX).contains(&date.year)
        || !(1..=12).contains(&date.month)
        || date.day == 0
        || date.day > datealgo::days_in_month(date.year, date.month)
        || time.hour > 23
        || time.minute > 59
        || time.second > 60
    {
        return Err(Error::Range);
    }
    let seconds =
        u64::from(time.hour) * 3600 + u64::from(time.minute) * 60 + u64::from(time.second.min(59));
    PlainDateTime {
        day: datealgo::date_to_rd((date.year, date.month, date.day)),
        nanosecond: seconds * NANOS_PER_SECOND as u64 + u64::from(time.nanosecond),
    }
    .checked()
}

pub(crate) fn offset_nanoseconds(offset: Offset) -> i64 {
    match offset {
        Offset::Z => 0,
        Offset::Nanoseconds(nanos) => nanos,
    }
}

#[cfg(test)]
pub(crate) fn strict_utc(input: &[u8]) -> Result<(), Error> {
    if !(20..=30).contains(&input.len()) || input.last() != Some(&b'Z') {
        return Err(Error::Syntax);
    }
    for (index, byte) in input[..19].iter().enumerate() {
        let separator = match index {
            4 | 7 => Some(b'-'),
            10 => Some(b'T'),
            13 | 16 => Some(b':'),
            _ => None,
        };
        if separator.map_or(!byte.is_ascii_digit(), |expected| *byte != expected) {
            return Err(Error::Syntax);
        }
    }
    if input[17] > b'5' {
        return Err(Error::Range);
    }
    if input.len() != 20
        && (input.len() < 22
            || input[19] != b'.'
            || !input[20..input.len() - 1].iter().all(u8::is_ascii_digit))
    {
        return Err(Error::Syntax);
    }
    Ok(())
}
