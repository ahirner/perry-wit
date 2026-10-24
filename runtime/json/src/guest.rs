//! Checked adapter from owned guest byte ranges to the safe JSON codec.
//!
//! Results pack an error code in the high word and a value/offset in the low word.
//! Codes: 0 success, 1 syntax, 2 surrogate, 3 depth, 4 capacity, 6 memory range/overlap,
//! 7 malformed UTF-8.

use crate::{Error, measure, populate};
use core::str;

use memory::GuestRange;
use perry_rt_sys::guest_memory as memory;

/// Validate strict UTF-8 JSON and return its required graph storage size in bytes.
///
/// # Safety
/// Nonempty inputs must be initialized guest-owned memory outside the library's
/// stack/data, and remain unchanged for this call. The guest must be single-threaded.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_measure(pointer: u32, length: u32) -> u64 {
    let Some(input) = GuestRange::new(pointer, length) else {
        return failure(6, 0);
    };
    // SAFETY: bounds are checked; guest ownership and immutability are the ABI contract.
    let input = unsafe { input.bytes() };
    let input = match str::from_utf8(input) {
        Ok(input) => input,
        Err(error) => return failure(7, error.valid_up_to()),
    };
    outcome(measure(input))
}

/// Populate a graph with absolute addresses in a disjoint guest output range.
/// Validation and capacity failures leave the output unchanged.
///
/// # Safety
/// Both ranges must be guest-owned storage outside library stack/data. The input
/// must be initialized and immutable, and the output must be exclusively writable during
/// this single-threaded call. Checked disjointness prevents input/output aliasing.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_populate(
    pointer: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u64 {
    let (input, destination) = match disjoint_ranges(pointer, length, output, capacity) {
        Ok(ranges) => ranges,
        Err(error) => return error,
    };
    // SAFETY: checked bounds and the caller's initialized, immutable input contract.
    let input = unsafe { input.bytes() };
    let input = match str::from_utf8(input) {
        Ok(input) => input,
        Err(error) => return failure(7, error.valid_up_to()),
    };
    let required = match measure(input) {
        Ok(required) => required,
        Err(error) => return codec_failure(error),
    };
    if required > destination.length {
        return failure(4, 0);
    }
    // SAFETY: output is in bounds, disjoint from input, and exclusively guest-owned.
    let destination = unsafe { destination.bytes_mut() };
    outcome(populate(input, destination, output).map(|root| root as usize))
}

/// Populate a JSON graph from an input previously validated and sized by `json_measure`.
///
/// # Safety
/// The input must be unchanged since a successful `json_measure` call, and capacity must
/// cover that measured size. Both ranges obey `json_populate`'s ownership contract. Bounds
/// and disjointness remain checked, while malformed-input failure atomicity belongs to the
/// checked `json_populate` entry point.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_populate_measured(
    pointer: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u64 {
    let (input, destination) = match disjoint_ranges(pointer, length, output, capacity) {
        Ok(ranges) => ranges,
        Err(error) => return error,
    };
    // SAFETY: successful measurement establishes UTF-8; the caller preserves the input
    // and provides exclusive output, whose bounds and disjointness were checked above.
    let input = unsafe { str::from_utf8_unchecked(input.bytes()) };
    let destination = unsafe { destination.bytes_mut() };
    outcome(populate(input, destination, output).map(|root| root as usize))
}

fn disjoint_ranges(
    pointer: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> Result<(GuestRange, GuestRange), u64> {
    let input = GuestRange::new(pointer, length).ok_or_else(|| failure(6, 0))?;
    let destination = GuestRange::new(output, capacity).ok_or_else(|| failure(6, 0))?;
    if input.overlaps(destination) {
        return Err(failure(6, 0));
    }
    Ok((input, destination))
}

fn outcome(result: Result<usize, Error>) -> u64 {
    match result {
        Ok(value) => u32::try_from(value).map_or_else(|_| failure(4, 0), u64::from),
        Err(error) => codec_failure(error),
    }
}

fn codec_failure(error: Error) -> u64 {
    match error {
        Error::Syntax(offset) => failure(1, offset),
        Error::UnpairedSurrogate(offset) => failure(2, offset),
        Error::Depth(offset) => failure(3, offset),
        Error::Capacity => failure(4, 0),
    }
}

fn failure(code: u32, detail: usize) -> u64 {
    (u64::from(code) << 32) | u64::from(detail as u32)
}
