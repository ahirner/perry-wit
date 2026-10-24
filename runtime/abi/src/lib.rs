//! Allocation-free layouts and algorithms shared by the compiler and runtime.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

pub mod json;
pub mod layout;
pub mod number;
pub mod utf8;
pub mod whitespace;
pub use utf8::scalars_length_wide as scalars_length;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}

/// Callback events and copy results of the canonical ABI.
pub const EVENT_SUBTASK: u32 = 1;
pub const EVENT_STREAM_READ: u32 = 2;
pub const EVENT_STREAM_WRITE: u32 = 3;
pub const EVENT_FUTURE_READ: u32 = 4;
pub const SUBTASK_RETURNED: u32 = 2;
pub const BLOCKED: u32 = 0xffff_ffff;
pub const COMPLETED: u32 = 0;
pub const DROPPED: u32 = 1;

/// `TypeError`, `RangeError`, `Error`, `ReferenceError` and `SyntaxError` statuses and layout.
pub use layout::exception::{
    ERROR, RANGE_ERROR, REFERENCE_ERROR, SYNTAX_ERROR, TAG_NUMBER, TAG_STRING, THROWN_VALUE,
    TYPE_ERROR,
};
pub const EXCEPTION_KIND: usize = layout::exception::KIND as usize;
pub const EXCEPTION_TAG: usize = layout::exception::TAG as usize;
pub const EXCEPTION_VALUE: usize = layout::exception::VALUE as usize;
/// Bits of an omitted optional field in a `number | undefined` slot.
pub use layout::descriptor::OMITTED_BITS;
/// Signalling-NaN bit pattern representing `undefined` in a `number | undefined` slot.
pub use layout::descriptor::UNDEFINED_BITS;
/// Offset of an array's data address ([layout::array::DATA]).
pub const ARRAY_DATA: u32 = layout::array::DATA;
/// Offset of a `Uint8Array`'s data address in the wasm32 byte-view layout.
pub const BYTES_DATA: usize = 4;

#[cfg(test)]
mod tests {
    use crate::utf8::{scalars_length, scalars_length_wide};

    #[test]
    fn scalar_counts_cross_word_boundaries() {
        for prefix in 0..24 {
            for suffix in 0..24 {
                let text = std::format!("{}é漢🙂\0{}", "a".repeat(prefix), "z".repeat(suffix));
                assert_eq!(
                    scalars_length(text.as_bytes()) as usize,
                    text.chars().count()
                );
                assert_eq!(
                    scalars_length_wide(text.as_bytes()),
                    scalars_length(text.as_bytes())
                );
            }
        }
        assert_eq!(scalars_length(b""), 0);
        assert_eq!(scalars_length_wide(b""), 0);
    }
}
