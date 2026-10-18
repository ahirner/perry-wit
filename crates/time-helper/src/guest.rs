//! Checked time-codec ABI. Values occupy 16 bytes; results pack status above length.
//! Instant stores little-endian i128 epoch nanoseconds. PlainDateTime stores an i32
//! epoch day, four padding bytes, and u64 nanoseconds within that day.
//! Status: 0 success, 1 syntax, 2 range, 3 annotation, 4 capacity, 6 memory/overlap.
//! Scalar accessors write eight bytes on success and use the same status convention.

use crate::{Error, Instant, PlainDateTime};

#[path = "../../perry-abi/src/layout/time.rs"]
#[expect(unreachable_pub, reason = "Shared codes are public in perry-abi.")]
mod codes;
use codes::{DatePart, DurationPart, Overflow, Unit};

#[path = "../../../src/helpers/guest_memory.rs"]
mod memory;
use memory::GuestRange;

/// # Safety
/// Input must be initialized immutable guest-owned storage outside helper stack/data.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_date_parse(input: u32, length: u32) -> f64 {
    let Ok(input) = GuestRange::new(input, length) else {
        return f64::NAN;
    };
    // SAFETY: checked bounds and caller's immutable ownership contract.
    crate::parse_date_milliseconds(unsafe { input.bytes() }).unwrap_or(f64::NAN)
}

/// # Safety
/// Output must be exclusively guest-owned outside helper stack/data during this call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_date_iso(value: f64, output: u32, capacity: u32) -> u64 {
    // SAFETY: the caller owns output; transform checks its guest-memory range.
    unsafe {
        transform([], output, capacity, |[], output| {
            Instant::from_epoch_milliseconds(value)?.format_milliseconds(output)
        })
    }
}

/// # Safety
/// Input/output must be guest-owned, initialized/exclusive respectively, outside
/// helper stack/data, and remain valid during this single-threaded call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_instant_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; transform checks ranges and overlap.
    unsafe {
        transform([(input, length)], output, 16, |[input], output| {
            let value = Instant::parse(input)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_utc_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, length)], output, 16, |[input], output| {
            let value = Instant::parse_utc(input)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Output must be exclusively guest-owned outside helper stack/data during this call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_instant_from_ms(value: f64, output: u32) -> u64 {
    // SAFETY: no input range; output ownership is the caller's contract.
    unsafe {
        transform([], output, 16, |[], output| {
            let value = Instant::from_epoch_milliseconds(value)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Input must be initialized; output must be exclusively owned and disjoint.
/// Both ranges must be outside helper stack/data.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_instant_ms(input: u32, output: u32) -> u64 {
    unsafe {
        scalar([(input, 16)], output, |[input]| {
            Ok(instant(input)?.epoch_milliseconds() as f64)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_instant_format(input: u32, output: u32, capacity: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, 16)], output, capacity, |[input], output| {
            instant(input)?.format(output)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, length)], output, 16, |[input], output| {
            write_plain(PlainDateTime::parse(input)?, output);
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_date_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, length)], output, 16, |[input], output| {
            write_plain(PlainDateTime::parse_plain_date(input)?, output);
            Ok(16)
        })
    }
}

/// # Safety
/// Output must be exclusively guest-owned outside helper stack/data during this call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_date_from_fields(
    year: f64,
    month: f64,
    day: f64,
    constrain: u32,
    output: u32,
) -> u64 {
    // SAFETY: no input range; output ownership is the caller's contract.
    unsafe {
        transform([], output, 16, |[], output| {
            let overflow = Overflow::try_from(constrain).map_err(|_| Error::Range)?;
            write_plain(
                PlainDateTime::from_fields(year, month, day, overflow == Overflow::Constrain)?,
                output,
            );
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_add_days(input: u32, days: f64, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, 16)], output, 16, |[input], output| {
            if !days.is_finite() || days.abs() > 200_000_002.0 || days != (days as i64) as f64 {
                return Err(Error::Range);
            }
            write_plain(plain(input)?.add_days(days as i64)?, output);
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_add_calendar(
    input: u32,
    years: f64,
    months: f64,
    weeks: f64,
    days: f64,
    constrain: u32,
    output: u32,
) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, 16)], output, 16, |[input], output| {
            let overflow = Overflow::try_from(constrain).map_err(|_| Error::Range)?;
            let mut sign = 0i64;
            let mut to_int = |v: f64| -> Result<i64, Error> {
                if !v.is_finite() || v.abs() > 200_000_002.0 || v != (v as i64) as f64 {
                    return Err(Error::Range);
                }
                let n = v as i64;
                if n != 0 {
                    let s = n.signum();
                    if sign != 0 && sign != s {
                        return Err(Error::Range);
                    }
                    sign = s;
                }
                Ok(n)
            };
            let y = to_int(years)?;
            let m = to_int(months)?;
            let w = to_int(weeks)?;
            let d = to_int(days)?;
            write_plain(
                plain(input)?.add_calendar(y, m, w, d, overflow == Overflow::Constrain)?,
                output,
            );
            Ok(16)
        })
    }
}

/// # Safety
/// Inputs must be initialized; output must be exclusively owned and disjoint from both.
/// All ranges must be outside helper stack/data.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_until_part(
    a: u32,
    b: u32,
    largest_unit: u32,
    part: u32,
    output: u32,
) -> u64 {
    unsafe {
        scalar([(a, 16), (b, 16)], output, |[a, b]| {
            let unit = Unit::try_from(largest_unit).map_err(|_| Error::Range)?;
            let part = DurationPart::try_from(part).map_err(|_| Error::Range)?;
            let (a, b) = (plain(a)?, plain(b)?);
            let (years, months, weeks, days) = match unit {
                Unit::Year | Unit::Month => {
                    let (months, days) = a.until_months_and_days(b);
                    if unit == Unit::Year {
                        (months / 12, months % 12, 0, days)
                    } else {
                        (0, months, 0, days)
                    }
                }
                Unit::Day | Unit::Week => {
                    let days = b.day_and_nanosecond().0 - a.day_and_nanosecond().0;
                    if unit == Unit::Week {
                        (0, 0, days / 7, days % 7)
                    } else {
                        (0, 0, 0, days)
                    }
                }
            };
            Ok(f64::from(match part {
                DurationPart::Years => years,
                DurationPart::Months => months,
                DurationPart::Weeks => weeks,
                DurationPart::Days => days,
            }))
        })
    }
}

/// # Safety
/// Input must be initialized; output must be exclusively owned and disjoint.
/// Both ranges must be outside helper stack/data.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_part(input: u32, part: u32, output: u32) -> u64 {
    unsafe {
        scalar([(input, 16)], output, |[input]| {
            let part = DatePart::try_from(part).map_err(|_| Error::Range)?;
            let value = plain(input)?;
            let (year, month, day) = value.date();
            let (hour, minute, second, fraction) = value.time();
            Ok(match part {
                DatePart::Year => f64::from(year),
                DatePart::Month => f64::from(month),
                DatePart::Day => f64::from(day),
                DatePart::Hour => f64::from(hour),
                DatePart::Minute => f64::from(minute),
                DatePart::Second => f64::from(second),
                DatePart::Millisecond => f64::from(fraction / 1_000_000),
                DatePart::Microsecond => f64::from(fraction / 1_000 % 1_000),
                DatePart::Nanosecond => f64::from(fraction % 1_000),
                DatePart::Weekday => f64::from(value.weekday()),
            })
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_format(input: u32, output: u32, capacity: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, 16)], output, capacity, |[input], output| {
            plain(input)?.format(output)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn time_plain_date_format(
    input: u32,
    output: u32,
    capacity: u32,
) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform([(input, 16)], output, capacity, |[input], output| {
            plain(input)?.format_plain_date(output)
        })
    }
}

/// UTC accessor; invalid times produce NaN.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn time_date_part(ms: f64, part: u32) -> f64 {
    crate::date_part(ms, part)
}

/// UTC construction with overflowing calendar fields.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn time_date_utc(
    year: f64,
    month: f64,
    day: f64,
    hour: f64,
    minute: f64,
    second: f64,
    ms: f64,
) -> f64 {
    crate::date_utc([year, month, day, hour, minute, second, ms])
}

/// Pure setter calculation; compiled code owns and updates the Date object.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn time_date_set(
    time: f64,
    part: u32,
    count: u32,
    a: f64,
    b: f64,
    c: f64,
    d: f64,
) -> f64 {
    crate::date_set(time, part, count as usize, [a, b, c, d])
}

unsafe fn transform<const N: usize>(
    inputs: [(u32, u32); N],
    output: u32,
    capacity: u32,
    operation: impl FnOnce([&[u8]; N], &mut [u8]) -> Result<usize, Error>,
) -> u64 {
    let Ok(output) = GuestRange::new(output, capacity) else {
        return 6 << 32;
    };
    let mut borrowed = [&[][..]; N];
    for (bytes, (pointer, length)) in borrowed.iter_mut().zip(inputs) {
        let Ok(input) = GuestRange::new(pointer, length) else {
            return 6 << 32;
        };
        if input.overlaps(output) {
            return 6 << 32;
        }
        *bytes = unsafe { input.bytes() };
    }
    // SAFETY: ranges are in bounds and disjoint; the caller supplies initialization
    // and exclusive output ownership outside the helper's own stack/data.
    let result = operation(borrowed, unsafe { output.bytes_mut() });
    match result {
        Ok(length) => length as u64,
        Err(error) => {
            (match error {
                Error::Syntax => 1,
                Error::Range => 2,
                Error::UnsupportedAnnotation => 3,
                Error::Capacity => 4,
            }) << 32
        }
    }
}

unsafe fn scalar<const N: usize>(
    inputs: [(u32, u32); N],
    output: u32,
    operation: impl FnOnce([&[u8]; N]) -> Result<f64, Error>,
) -> u64 {
    unsafe {
        transform(inputs, output, 8, |inputs, output| {
            output.copy_from_slice(&operation(inputs)?.to_le_bytes());
            Ok(8)
        })
    }
}

fn instant(input: &[u8]) -> Result<Instant, Error> {
    let bytes = input.try_into().map_err(|_| Error::Capacity)?;
    Instant::from_epoch_nanoseconds(i128::from_le_bytes(bytes))
}

fn plain(input: &[u8]) -> Result<PlainDateTime, Error> {
    let day = i32::from_le_bytes(input[..4].try_into().map_err(|_| Error::Capacity)?);
    let nanosecond = u64::from_le_bytes(input[8..].try_into().map_err(|_| Error::Capacity)?);
    PlainDateTime::from_day_and_nanosecond(day, nanosecond)
}

fn write_plain(value: PlainDateTime, output: &mut [u8]) {
    let (day, nanosecond) = value.day_and_nanosecond();
    output[..4].copy_from_slice(&day.to_le_bytes());
    output[4..8].fill(0);
    output[8..].copy_from_slice(&nanosecond.to_le_bytes());
}
