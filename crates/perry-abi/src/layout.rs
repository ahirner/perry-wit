//! Wasm32 aggregate layouts shared by generated code and linked helpers.
#[path = "layout/time.rs"]
pub mod time;

pub mod record {
    pub const ORDER: u32 = 0;
    pub const JSON: u32 = 4;
    pub const HEADER: u32 = 8;
    pub const SLOT: u32 = super::array::SLOT;
    pub const NUMERIC_SLOT: u32 = super::array::NUMERIC_SLOT;

    pub const fn field(index: usize) -> u32 {
        HEADER + SLOT * index as u32
    }
}

pub mod array {
    pub const LEN: u32 = 0;
    pub const CAP: u32 = 4;
    pub const DATA: u32 = 8;
    pub const SLOT: u32 = 8;
    pub const NUMERIC_SLOT: u32 = 16;
    pub const SCALAR_ELEMENTS: u32 = 0;
    pub const REFERENCE_ELEMENTS: u32 = 1;
    pub const NUMERIC_ELEMENTS: u32 = 2;

    /// The element representation of an array, decoded from the `references` flag compiled code
    /// passes (`*_ELEMENTS`).
    #[derive(Clone, Copy, PartialEq, Eq)]
    #[repr(u32)]
    pub enum Elements {
        /// Opaque 8-byte slots: `f64`, booleans or fixed-width integers.
        Scalar = SCALAR_ELEMENTS,
        /// 8-byte slots holding a reference in the low half, traced precisely.
        References = REFERENCE_ELEMENTS,
        /// Opaque 16-byte slots holding a numeric tag and payload.
        Numeric = NUMERIC_ELEMENTS,
    }

    impl Elements {
        /// Decode the ABI flag; any other value is a compiler bug, so it traps.
        pub fn decode(references: u32) -> Self {
            match references {
                SCALAR_ELEMENTS => Self::Scalar,
                REFERENCE_ELEMENTS => Self::References,
                NUMERIC_ELEMENTS => Self::Numeric,
                _ => panic!("invalid array element kind"),
            }
        }

        /// Bytes per element slot.
        pub fn stride(self) -> u32 {
            match self {
                Self::Scalar | Self::References => SLOT,
                Self::Numeric => NUMERIC_SLOT,
            }
        }

        /// `u64` words per element slot.
        pub fn words(self) -> usize {
            (self.stride() / SLOT) as usize
        }
    }
}

pub mod variant {
    pub const TAG: u32 = 0;
    pub const OWNER: u32 = 4;
    pub const VALUE: u32 = 8;
    pub const SIZE: u32 = 16;
    pub const JSON: u32 = SIZE;
}

/// Native helpers and Wasm code share this definition, including string headers.
pub mod string {
    pub const LEN: u32 = 0;
    pub const SCALARS: u32 = 4;
    pub const DATA: u32 = 8;
    pub const BUFFER: u32 = 12;
    pub const OWNER: u32 = 16;
    pub const CURSOR_SCALAR: u32 = 24;
    pub const CURSOR_BYTE: u32 = 28;
    pub const SIZE: u32 = 32;

    #[repr(C)]
    pub struct AppendBuffer {
        pub cap: u32,
        pub used: u32,
    }

    #[repr(C)]
    pub struct Header {
        pub len: u32,
        pub scalars: u32,
        pub data: *mut u8,
        pub buffer: *mut AppendBuffer,
        pub owner: *mut u8,
        pub offset: u32,
        pub cursor_scalar: u32,
        pub cursor_byte: u32,
    }
}

/// Headers shared by generated async frames and promise cancellation traversal.
pub mod async_frame {
    pub const KIND: u32 = 0;
    pub const STATE: u32 = 4;
    pub const PROMISE: u32 = 8;
    pub const AWAITED: u32 = 12;
    pub const PIN: u32 = 16;
    pub const OBSERVER: u32 = 24;
    pub const OBSERVER_SIZE: u32 = 32;
    pub const SLOTS: u32 = OBSERVER + OBSERVER_SIZE;
}

pub mod promise {
    pub const STATE: u32 = 0;
    pub const STATUS: u32 = 4;
    pub const VALUE: u32 = 8;
    pub const FULFILLED: u32 = 1;
    pub const REJECTED: u32 = 2;
    pub const COMBINATOR_KIND: u32 = u32::MAX;
}

pub mod exception {
    pub const KIND: u32 = 0;
    pub const TAG: u32 = 4;
    pub const VALUE: u32 = 8;
    pub const CAUSE: u32 = 16;
    pub const TYPE_ERROR: u32 = 1;
    pub const RANGE_ERROR: u32 = 2;
    pub const ERROR: u32 = 3;
    pub const REFERENCE_ERROR: u32 = 6;
    pub const SYNTAX_ERROR: u32 = 7;
    pub const TAG_UNDEFINED: u32 = 5;
    pub const TAG_NULL: u32 = 6;
    pub const TAG_JSON: u32 = 7;
    pub const THROWN_VALUE: u32 = 4;
    pub const TAG_NUMBER: u32 = 1;
    pub const TAG_BOOLEAN: u32 = 2;
    pub const TAG_STRING: u32 = 3;
    pub const TAG_BIGINT: u32 = 4;
}

pub mod foreign {
    pub const AREA: u32 = 0;
    pub const OWNERS: u32 = 4;
    pub const KIND: u32 = 8;
    pub const RETURNED: u32 = 12;
    pub const SCOPE: u32 = 16;
    pub const SIZE: u32 = 20;
}

pub mod roots {
    pub const DATA: u32 = 0;
    pub const LENGTH: u32 = 4;
    pub const CAPACITY: u32 = 8;
}

/// Compiler-emitted native layout descriptors read by `perry-std` cause reflection.
pub mod descriptor {
    pub const NUMBER: u32 = 1;
    pub const BOOLEAN: u32 = 2;
    pub const STRING: u32 = 3;
    pub const RECORD: u32 = 4;
    pub const ARRAY: u32 = 5;
    pub const EXCEPTION: u32 = 6;
    pub const OPTIONAL: u32 = 7;
    pub const SIGNED_INT: u32 = 8;
    pub const UNSIGNED_INT: u32 = 9;
    pub const NULL_NUMBER_BITS: u64 = 0x7ff0_0000_0000_0002;
}

#[path = "layout/regex.rs"]
pub mod regex;

pub mod numeric {
    pub const FLOAT: i32 = 0;
    pub const SIGNED: i32 = 1;
    pub const UNSIGNED: i32 = 2;
    pub const UNDEFINED: i32 = 3;
    pub const NULL: i32 = 4;
    pub const OMITTED: i32 = 5;
    pub const U8: i32 = 6;
    pub const S8: i32 = 7;
    pub const U16: i32 = 8;
    pub const S16: i32 = 9;
    pub const U32: i32 = 10;
    pub const S32: i32 = 11;
    pub const U64: i32 = 12;
    pub const S64: i32 = 13;

    /// A decoded numeric tag. Compiled code passes tags as raw `i32`, so helpers decode them
    /// once with `TryFrom` and then match exhaustively instead of guessing on unknown values.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Tag {
        Float,
        Signed,
        Unsigned,
        Undefined,
        Null,
        Omitted,
        U8,
        S8,
        U16,
        S16,
        U32,
        S32,
        U64,
        S64,
    }

    impl TryFrom<i32> for Tag {
        /// The rejected raw tag.
        type Error = i32;

        fn try_from(tag: i32) -> Result<Self, i32> {
            Ok(match tag {
                FLOAT => Self::Float,
                SIGNED => Self::Signed,
                UNSIGNED => Self::Unsigned,
                UNDEFINED => Self::Undefined,
                NULL => Self::Null,
                OMITTED => Self::Omitted,
                U8 => Self::U8,
                S8 => Self::S8,
                U16 => Self::U16,
                S16 => Self::S16,
                U32 => Self::U32,
                S32 => Self::S32,
                U64 => Self::U64,
                S64 => Self::S64,
                other => return Err(other),
            })
        }
    }
}
