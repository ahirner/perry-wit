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

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn digits(&mut self, count: usize) -> Result<u32, Error> {
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
}

pub(crate) fn record(input: &[u8]) -> Result<ParsedRecord, Error> {
    if input.contains(&b'[') {
        return Err(Error::UnsupportedAnnotation);
    }
    let mut cur = Cursor::new(input);
    let year = match cur.peek() {
        Some(sign @ (b'+' | b'-')) => {
            cur.pos += 1;
            let raw = cur.digits(6)? as i32;
            if sign == b'-' {
                if raw == 0 {
                    return Err(Error::Syntax);
                }
                -raw
            } else {
                raw
            }
        }
        _ => cur.digits(4)? as i32,
    };
    let (month, day) = if cur.eat(b'-') {
        let month = cur.digits(2)? as u8;
        if !cur.eat(b'-') {
            return Err(Error::Syntax);
        }
        let day = cur.digits(2)? as u8;
        (month, day)
    } else {
        let month = cur.digits(2)? as u8;
        let day = cur.digits(2)? as u8;
        (month, day)
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
    let hour = cur.digits(2)? as u8;
    let mut minute = 0u8;
    let mut second = 0u8;
    if cur.eat(b':') {
        minute = cur.digits(2)? as u8;
        if cur.eat(b':') {
            second = cur.digits(2)? as u8;
        }
    } else if cur.peek().is_some_and(|b| b.is_ascii_digit()) {
        minute = cur.digits(2)? as u8;
        if cur.peek().is_some_and(|b| b.is_ascii_digit()) {
            second = cur.digits(2)? as u8;
        }
    }
    let nanosecond = if cur.eat(b'.') || cur.eat(b',') {
        cur.fraction_nanos()?
    } else {
        0
    };
    let time = Some(TimeRecord {
        hour,
        minute,
        second,
        nanosecond,
    });
    let offset = match cur.peek() {
        Some(b'Z' | b'z') => {
            cur.pos += 1;
            Some(Offset::Z)
        }
        Some(sign @ (b'+' | b'-')) => {
            cur.pos += 1;
            let off_hour = cur.digits(2)? as u8;
            let mut off_min = 0u8;
            let mut off_sec = 0u8;
            let mut has_sec = false;
            if cur.eat(b':') {
                off_min = cur.digits(2)? as u8;
                if cur.eat(b':') {
                    off_sec = cur.digits(2)? as u8;
                    has_sec = true;
                }
            } else if cur.peek().is_some_and(|b| b.is_ascii_digit()) {
                off_min = cur.digits(2)? as u8;
                if cur.peek().is_some_and(|b| b.is_ascii_digit()) {
                    off_sec = cur.digits(2)? as u8;
                    has_sec = true;
                }
            }
            let off_nano = if has_sec && (cur.eat(b'.') || cur.eat(b',')) {
                cur.fraction_nanos()?
            } else {
                0
            };
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
