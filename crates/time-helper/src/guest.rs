//! Checked time-codec ABI. Values occupy 16 bytes; results pack status above length.
//! Instant stores little-endian i128 epoch nanoseconds. PlainDateTime stores an i32
//! epoch day, four padding bytes, and u64 nanoseconds within that day.
//! Status: 0 success, 1 syntax, 2 range, 3 annotation, 4 capacity, 6 memory/overlap.
//! Scalar accessors return NaN on invalid storage; valid values are always finite.

use crate::{Error, Instant, PlainDateTime};

#[path = "../../../src/helpers/guest_memory.rs"]
mod memory;
use memory::GuestRange;

/// # Safety
/// Output must be exclusively guest-owned outside helper stack/data during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_date_iso(value: f64, output: u32, capacity: u32) -> u64 {
    // SAFETY: the caller owns output; transform checks its guest-memory range.
    unsafe {
        transform(0, 0, output, capacity, |_, output| {
            Instant::from_epoch_milliseconds(value)?.format_milliseconds(output)
        })
    }
}

/// # Safety
/// Input/output must be guest-owned, initialized/exclusive respectively, outside
/// helper stack/data, and remain valid during this single-threaded call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_instant_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; transform checks ranges and overlap.
    unsafe {
        transform(input, length, output, 16, |input, output| {
            let value = Instant::parse(input)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_utc_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform(input, length, output, 16, |input, output| {
            let value = Instant::parse_utc(input)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Output must be exclusively guest-owned outside helper stack/data during this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_instant_from_ms(value: f64, output: u32) -> u64 {
    // SAFETY: no input range; output ownership is the caller's contract.
    unsafe {
        transform(0, 0, output, 16, |_, output| {
            let value = Instant::from_epoch_milliseconds(value)?;
            output.copy_from_slice(&value.epoch_nanoseconds().to_le_bytes());
            Ok(16)
        })
    }
}

/// # Safety
/// Input must be initialized immutable guest-owned storage outside helper stack/data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_instant_ms(input: u32) -> f64 {
    let Ok(input) = GuestRange::new(input, 16) else {
        return f64::NAN;
    };
    // SAFETY: checked bounds and caller's immutable ownership contract.
    instant(unsafe { input.bytes() }).map_or(f64::NAN, |value| value.epoch_milliseconds() as f64)
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_instant_format(input: u32, output: u32, capacity: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform(input, 16, output, capacity, |input, output| {
            instant(input)?.format(output)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_plain_parse(input: u32, length: u32, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform(input, length, output, 16, |input, output| {
            write_plain(PlainDateTime::parse(input)?, output);
            Ok(16)
        })
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_plain_add_days(input: u32, days: f64, output: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform(input, 16, output, 16, |input, output| {
            if !days.is_finite() || days.abs() > 200_000_002.0 || days != (days as i64) as f64 {
                return Err(Error::Range);
            }
            write_plain(plain(input)?.add_days(days as i64)?, output);
            Ok(16)
        })
    }
}

/// # Safety
/// Input must be initialized immutable guest-owned storage outside helper stack/data.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_plain_part(input: u32, part: u32) -> f64 {
    let Ok(input) = GuestRange::new(input, 16) else {
        return f64::NAN;
    };
    // SAFETY: checked bounds and caller's immutable ownership contract.
    let Ok(value) = plain(unsafe { input.bytes() }) else {
        return f64::NAN;
    };
    let (year, month, day) = value.date();
    let (hour, minute, second, fraction) = value.time();
    match part {
        0 => f64::from(year),
        1 => f64::from(month),
        2 => f64::from(day),
        3 => f64::from(hour),
        4 => f64::from(minute),
        5 => f64::from(second),
        6 => f64::from(fraction / 1_000_000),
        7 => f64::from(fraction / 1_000 % 1_000),
        8 => f64::from(fraction % 1_000),
        _ => f64::NAN,
    }
}

/// # Safety
/// Same caller-owned, disjoint memory contract as time_instant_parse.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn time_plain_format(input: u32, output: u32, capacity: u32) -> u64 {
    // SAFETY: forwarded guest ownership contract; ranges and overlap are checked.
    unsafe {
        transform(input, 16, output, capacity, |input, output| {
            plain(input)?.format(output)
        })
    }
}

unsafe fn transform(
    input: u32,
    length: u32,
    output: u32,
    capacity: u32,
    operation: impl FnOnce(&[u8], &mut [u8]) -> Result<usize, Error>,
) -> u64 {
    let (Ok(input), Ok(output)) = (
        GuestRange::new(input, length),
        GuestRange::new(output, capacity),
    ) else {
        return 6 << 32;
    };
    if input.overlaps(output) {
        return 6 << 32;
    }
    // SAFETY: ranges are in bounds and disjoint; the caller supplies initialization
    // and exclusive output ownership outside the helper's own stack/data.
    let result = operation(unsafe { input.bytes() }, unsafe { output.bytes_mut() });
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
