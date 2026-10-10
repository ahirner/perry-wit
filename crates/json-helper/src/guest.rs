//! Checked adapter from owned guest byte ranges to the safe JSON codec.
//!
//! Results pack an error code in the high word and a value/offset in the low word.
//! Codes: 0 success, 1 syntax, 2 surrogate, 3 depth, 4 capacity, 6 memory range/overlap,
//! 7 malformed UTF-8.

use crate::{Error, measure, populate};
use core::str;

#[path = "../../../src/helpers/guest_memory.rs"]
mod memory;
use memory::GuestRange;

/// Measure graph storage for strict UTF-8 JSON without changing guest memory.
///
/// # Safety
/// Nonempty inputs must be initialized guest-owned memory outside the helper's
/// stack/data, and remain unchanged for this call. The guest must be single-threaded.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_measure(pointer: u32, length: u32) -> u64 {
    let Ok(input) = GuestRange::new(pointer, length) else {
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
/// Both ranges must be guest-owned storage outside helper stack/data. The input
/// must be initialized and immutable; output must be exclusively writable during
/// this single-threaded call. Checked disjointness prevents input/output aliasing.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn json_populate(
    pointer: u32,
    length: u32,
    output: u32,
    capacity: u32,
) -> u64 {
    let (Ok(input), Ok(destination)) = (
        GuestRange::new(pointer, length),
        GuestRange::new(output, capacity),
    ) else {
        return failure(6, 0);
    };
    if input.overlaps(destination) {
        return failure(6, 0);
    }
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
