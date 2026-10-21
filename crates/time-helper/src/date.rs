//! ECMAScript ISO Date strings in the guest's UTC time zone.

use crate::{Error, parse::Cursor};

/// Parses ISO year, year-month, date, or date-time forms, accepting `24:00` and normalizing
/// days through 31 within a valid month.
/// Offset-free date-times use UTC, the guest's supported local time zone.
/// A four-digit year (or signed six-digit year) followed by end, '-' or 'T'
/// identifies the ISO family, and other inputs return `UnsupportedDateFormat`.
pub(crate) fn parse_date_milliseconds(input: &[u8]) -> Result<f64, Error> {
    let mut cur = Cursor::new(input);
    let year_end = if matches!(cur.peek(), Some(b'+' | b'-')) {
        7
    } else {
        4
    };
    if !matches!(input.get(year_end), None | Some(b'-' | b'T')) {
        return Err(Error::UnsupportedDateFormat);
    }
    let year = cur.year().map_err(|_| {
        if cur.pos == year_end {
            Error::Syntax
        } else {
            Error::UnsupportedDateFormat
        }
    })?;
    let (mut month, mut day) = (1, 1);
    if cur.eat(b'-') {
        month = cur.digits(2)?;
        if cur.eat(b'-') {
            day = cur.digits(2)?;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(Error::Range);
    }
    let days = i64::from(datealgo::date_to_rd((year, month as u8, 1))) + i64::from(day - 1);
    let mut milliseconds = days * 86_400_000;
    if cur.pos != input.len() {
        if cur.pos != year_end + 6 || !cur.eat(b'T') {
            return Err(Error::Syntax);
        }
        let hour = cur.digits(2)?;
        if !cur.eat(b':') {
            return Err(Error::Syntax);
        }
        let minute = cur.digits(2)?;
        let (mut second, mut fraction) = (0, 0);
        if cur.eat(b':') {
            second = cur.digits(2)?;
            if cur.eat(b'.') {
                let start = cur.pos;
                while let Some(d @ b'0'..=b'9') = cur.peek() {
                    if cur.pos - start < 3 {
                        fraction += u32::from(d - b'0') * [100, 10, 1][cur.pos - start];
                    }
                    cur.pos += 1;
                }
                if cur.pos == start {
                    return Err(Error::Syntax);
                }
            }
        }
        if hour > 24
            || minute > 59
            || second > 59
            || (hour == 24 && (minute | second | fraction) != 0)
        {
            return Err(Error::Range);
        }
        milliseconds += i64::from(((hour * 60 + minute) * 60 + second) * 1000 + fraction);
        match cur.peek() {
            Some(b'Z') => cur.pos += 1,
            Some(sign @ (b'+' | b'-')) => {
                cur.pos += 1;
                let hour = cur.digits(2)?;
                let colon = cur.eat(b':');
                let minute = cur.digits(2)?;
                if !colon || hour > 23 || minute > 59 {
                    return Err(Error::Range);
                }
                let offset = i64::from(hour * 60 + minute) * 60_000;
                milliseconds -= if sign == b'+' { offset } else { -offset };
            }
            None => {}
            _ => return Err(Error::Syntax),
        }
    }
    if cur.pos != input.len() {
        return Err(Error::Syntax);
    }
    if milliseconds.abs() > DATE_LIMIT as i64 {
        return Err(Error::Range);
    }
    Ok(milliseconds as f64)
}

const DAY_MS: i128 = 86_400_000;
const DATE_LIMIT: i128 = 8_640_000_000_000_000;

/// Date UTC fields: year, zero-based month, date, weekday, hour, minute, second, millisecond.
pub(crate) fn date_part(ms: f64, part: u32) -> f64 {
    if !ms.is_finite() || ms.abs() > DATE_LIMIT as f64 {
        return f64::NAN;
    }
    let ms = ms as i64;
    let days = ms.div_euclid(DAY_MS as i64) as i32;
    let (year, month, day) = datealgo::rd_to_date(days);
    match part {
        0 => f64::from(year),
        1 => f64::from(month - 1),
        2 => f64::from(day),
        3 => f64::from(datealgo::rd_to_weekday(days) % 7),
        4 => (ms.div_euclid(3_600_000).rem_euclid(24)) as f64,
        5 => (ms.div_euclid(60_000).rem_euclid(60)) as f64,
        6 => (ms.div_euclid(1000).rem_euclid(60)) as f64,
        7 => ms.rem_euclid(1000) as f64,
        8 => 0.0,
        _ => f64::NAN,
    }
}

/// UTC construction, with ECMAScript's 1900 adjustment for years 0 through 99.
pub(crate) fn date_utc(mut fields: [f64; 7]) -> f64 {
    if fields[0].is_finite() && (-1.0..100.0).contains(&fields[0]) {
        fields[0] = (fields[0] as i64 + 1900) as f64;
    }
    make_date(fields)
}

/// Computes a setter's new time value. Omitted trailing fields retain their old values.
pub(crate) fn date_set(time: f64, part: u32, count: usize, values: [f64; 4]) -> f64 {
    if count == 0 || part > 7 {
        return f64::NAN;
    }
    if part == 7 {
        let value = values[0];
        return if value.is_finite() && value.abs() <= DATE_LIMIT as f64 {
            (value as i64) as f64
        } else {
            f64::NAN
        };
    }
    let time = if part == 0 && time.is_nan() {
        0.0
    } else {
        time
    };
    let mut fields = [0.0; 7];
    for (i, field) in fields.iter_mut().enumerate() {
        *field = date_part(time, if i < 3 { i as u32 } else { i as u32 + 1 });
    }
    for (field, value) in fields[part as usize..]
        .iter_mut()
        .zip(values.into_iter().take(count))
    {
        *field = value;
    }
    make_date(fields)
}

fn make_date(fields: [f64; 7]) -> f64 {
    let mut parts = [0i128; 7];
    for (part, value) in parts.iter_mut().zip(fields) {
        if !value.is_finite() || value.abs() >= i64::MAX as f64 {
            return f64::NAN;
        }
        *part = i128::from(value as i64);
    }
    let [year, month, day, hours, minutes, seconds, milliseconds] = parts;
    let year = year + month.div_euclid(12);
    let month = month.rem_euclid(12) + 1;
    // March-based Gregorian eras allow overflowing month/day inputs before TimeClip.
    let year = year - i128::from(month <= 2);
    let era = year.div_euclid(400);
    let year_in_era = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let days = era * 146097 + year_in_era * 365 + year_in_era / 4 - year_in_era / 100
        + (153 * month + 2) / 5
        + day
        - 1
        - 719468;
    let result = days * DAY_MS + ((hours * 60 + minutes) * 60 + seconds) * 1000 + milliseconds;
    if result.abs() <= DATE_LIMIT {
        result as f64
    } else {
        f64::NAN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_normalization_and_invalid_recovery() {
        assert_eq!(date_utc([1970.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0]), 0.0);
        assert_eq!(date_utc([1970.0, 0.0, 0.0, 24.0, 0.0, 0.0, 0.0]), 0.0);
        assert_eq!(date_part(-1.0, 7), 999.0);
        assert_eq!(date_part(-1.0, 0), 1969.0);
        assert_eq!(date_set(f64::NAN, 0, 1, [1970.0, 0.0, 0.0, 0.0]), 0.0);
        assert!(date_set(f64::NAN, 1, 1, [0.0; 4]).is_nan());
        assert_eq!(
            date_utc([275760.0, 8.0, 13.0, 0.0, 0.0, 0.0, 0.0]),
            DATE_LIMIT as f64
        );
        assert_eq!(
            date_utc([-271821.0, 3.0, 20.0, 0.0, 0.0, 0.0, 0.0]),
            -(DATE_LIMIT as f64)
        );
    }
}
