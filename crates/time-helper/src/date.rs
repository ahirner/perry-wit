//! ECMAScript ISO Date strings in the guest's UTC time zone.

use crate::Error;

/// Parses ISO year, year-month, date, or date-time forms. Unlike Temporal,
/// Date permits 24:00 and normalizes days through 31 within a valid month.
/// Offset-free date-times use UTC, the guest's supported local time zone.
pub fn parse_date_milliseconds(input: &[u8]) -> Result<f64, Error> {
    let signed = matches!(input.first(), Some(b'+' | b'-'));
    let width = if signed { 6 } else { 4 };
    let mut year = digits(input, usize::from(signed), width)? as i32;
    if input.first() == Some(&b'-') {
        if year == 0 {
            return Err(Error::Syntax);
        }
        year = -year;
    }
    let mut end = width + usize::from(signed);
    let mut month = 1;
    let mut day = 1;
    if input.get(end) == Some(&b'-') {
        month = digits(input, end + 1, 2)?;
        end += 3;
        if input.get(end) == Some(&b'-') {
            day = digits(input, end + 1, 2)?;
            end += 3;
        }
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(Error::Range);
    }
    let days = i64::from(datealgo::date_to_rd((year, month as u8, 1))) + i64::from(day - 1);
    let mut milliseconds = days * 86_400_000;
    if end != input.len() {
        if end != width + usize::from(signed) + 6
            || input.get(end) != Some(&b'T')
            || input.get(end + 3) != Some(&b':')
        {
            return Err(Error::Syntax);
        }
        let hour = digits(input, end + 1, 2)?;
        let minute = digits(input, end + 4, 2)?;
        end += 6;
        let mut second = 0;
        let mut fraction = 0;
        if input.get(end) == Some(&b':') {
            second = digits(input, end + 1, 2)?;
            end += 3;
            if input.get(end) == Some(&b'.') {
                end += 1;
                let start = end;
                while input.get(end).is_some_and(u8::is_ascii_digit) {
                    if end - start < 3 {
                        fraction += u32::from(input[end] - b'0') * [100, 10, 1][end - start];
                    }
                    end += 1;
                }
                if end == start {
                    return Err(Error::Syntax);
                }
            }
        }
        if hour > 24
            || minute > 59
            || second > 59
            || (hour == 24 && (minute != 0 || second != 0 || fraction != 0))
        {
            return Err(Error::Range);
        }
        milliseconds += i64::from(((hour * 60 + minute) * 60 + second) * 1000 + fraction);
        match input.get(end) {
            Some(b'Z') => end += 1,
            Some(sign @ (b'+' | b'-')) => {
                let hour = digits(input, end + 1, 2)?;
                let minute = digits(input, end + 4, 2)?;
                if input.get(end + 3) != Some(&b':') || hour > 23 || minute > 59 {
                    return Err(Error::Range);
                }
                let offset = i64::from(hour * 60 + minute) * 60_000;
                milliseconds -= if *sign == b'+' { offset } else { -offset };
                end += 6;
            }
            None => {}
            _ => return Err(Error::Syntax),
        }
    }
    if end != input.len() {
        return Err(Error::Syntax);
    }
    if milliseconds.abs() > 8_640_000_000_000_000 {
        return Err(Error::Range);
    }
    Ok(milliseconds as f64)
}

fn digits(input: &[u8], start: usize, length: usize) -> Result<u32, Error> {
    input
        .get(start..start + length)
        .ok_or(Error::Syntax)?
        .iter()
        .try_fold(0, |value, digit| {
            if digit.is_ascii_digit() {
                Ok(value * 10 + u32::from(digit - b'0'))
            } else {
                Err(Error::Syntax)
            }
        })
}
