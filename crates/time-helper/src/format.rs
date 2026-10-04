use crate::{Error, PlainDateTime};

pub(crate) fn date_time(
    value: PlainDateTime,
    utc: bool,
    output: &mut [u8],
) -> Result<usize, Error> {
    date_time_precision(value, utc, 0, output)
}

pub(crate) fn date_time_precision(
    value: PlainDateTime,
    utc: bool,
    minimum_fraction: usize,
    output: &mut [u8],
) -> Result<usize, Error> {
    let mut bytes = [0; 33];
    let date_length = date(value.date(), &mut bytes);
    let (hour, minute, second, fraction) = value.time();
    let time = &mut bytes[date_length..];
    time[0] = b'T';
    decimal(u32::from(hour), &mut time[1..3]);
    time[3] = b':';
    decimal(u32::from(minute), &mut time[4..6]);
    time[6] = b':';
    decimal(u32::from(second), &mut time[7..9]);
    let mut length = date_length + 9;
    if fraction != 0 || minimum_fraction != 0 {
        bytes[length] = b'.';
        decimal(fraction, &mut bytes[length + 1..length + 10]);
        length += 10;
        while bytes[length - 1] == b'0' && length > date_length + 10 + minimum_fraction {
            length -= 1;
        }
    }
    if utc {
        bytes[length] = b'Z';
        length += 1;
    }
    let destination = output.get_mut(..length).ok_or(Error::Capacity)?;
    destination.copy_from_slice(&bytes[..length]);
    Ok(length)
}

pub(crate) fn date((year, month, day): (i32, u8, u8), output: &mut [u8]) -> usize {
    let year_length = if (0..=9999).contains(&year) {
        decimal(year as u32, &mut output[..4]);
        4
    } else {
        output[0] = if year < 0 { b'-' } else { b'+' };
        decimal(year.unsigned_abs(), &mut output[1..7]);
        7
    };
    output[year_length] = b'-';
    decimal(
        u32::from(month),
        &mut output[year_length + 1..year_length + 3],
    );
    output[year_length + 3] = b'-';
    decimal(
        u32::from(day),
        &mut output[year_length + 4..year_length + 6],
    );
    year_length + 6
}

fn decimal(mut number: u32, output: &mut [u8]) {
    for byte in output.iter_mut().rev() {
        *byte = b'0' + (number % 10) as u8;
        number /= 10;
    }
}
