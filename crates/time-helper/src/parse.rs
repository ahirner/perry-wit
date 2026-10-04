use super::{Error, NANOS_PER_SECOND, PlainDateTime};
use ixdtf::{
    encoding::Utf8,
    parsers::IxdtfParser,
    records::{DateRecord, IxdtfParseRecord, TimeRecord, UtcOffsetRecordOrZ},
};

pub(crate) fn record(input: &[u8]) -> Result<IxdtfParseRecord<'_, Utf8>, Error> {
    if input.contains(&b'[') {
        return Err(Error::UnsupportedAnnotation);
    }
    IxdtfParser::from_utf8(input)
        .parse()
        .map_err(|_| Error::Syntax)
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
    let fraction = time
        .fraction
        .map(|fraction| fraction.to_nanoseconds().ok_or(Error::Range))
        .transpose()?
        .unwrap_or(0);
    let seconds =
        u64::from(time.hour) * 3600 + u64::from(time.minute) * 60 + u64::from(time.second.min(59));
    PlainDateTime {
        day: datealgo::date_to_rd((date.year, date.month, date.day)),
        nanosecond: seconds * NANOS_PER_SECOND as u64 + u64::from(fraction),
    }
    .checked()
}

pub(crate) fn offset_nanoseconds(offset: UtcOffsetRecordOrZ) -> Result<i128, Error> {
    let UtcOffsetRecordOrZ::Offset(offset) = offset else {
        return Ok(0);
    };
    let fraction = offset
        .fraction()
        .map(|fraction| fraction.to_nanoseconds().ok_or(Error::Range))
        .transpose()?
        .unwrap_or(0);
    let seconds = i128::from(offset.hour()) * 3600
        + i128::from(offset.minute()) * 60
        + i128::from(offset.second().unwrap_or(0));
    Ok((seconds * NANOS_PER_SECOND + i128::from(fraction)) * offset.sign() as i128)
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
