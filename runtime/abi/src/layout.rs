//! Wasm32 aggregate layouts shared by generated code and runtime artifacts.
#[macro_use]
#[path = "layout/time.rs"]
pub mod time;

pub mod json {
    pub const NEXT: usize = 4;
}

pub mod record {
    pub const PACKED_FIELDS: usize = 8;
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
    #[repr(C)]
    pub struct Header {
        pub len: u32,
        pub cap: u32,
        pub data: *mut u64,
        pub holes: *mut u8,
    }

    pub const MIN_CAPACITY: u32 = 4;
    pub const LEN: u32 = 0;
    pub const CAP: u32 = 4;
    pub const DATA: u32 = 8;
    /// Nullable hole bitset: bit `index % 8` of byte `index / 8` marks a missing element.
    pub const HOLES: u32 = 12;
    pub const SLOT: u32 = 8;
    pub const NUMERIC_SLOT: u32 = 16;
    /// Bytes of a hole bitset covering `capacity` elements.
    pub const fn hole_bytes(capacity: u32) -> u32 {
        capacity.div_ceil(8)
    }

    /// Whether `index` is marked in the hole bitset `holes`; a null bitset has no holes.
    ///
    /// # Safety
    /// `holes` is null or covers `index`.
    pub unsafe fn is_hole(holes: *const u8, index: u32) -> bool {
        !holes.is_null() && unsafe { *holes.add((index / 8) as usize) } & (1 << (index % 8)) != 0
    }

    const SCALAR_ELEMENTS: u32 = 0;
    const REFERENCE_ELEMENTS: u32 = 1;
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
        /// Decode the ABI flag, trapping if `references` is outside `{0, 1}`.
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
    pub const RETAINED_SIZE: u32 = SIZE + 8;
}

/// String headers shared by runtime libraries and generated Wasm code.
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

    #[derive(Clone, Copy)]
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

/// Native storage descriptors shared by cause reflection and JSON.
/// Descriptor references carry absence flags in their low bits.
pub mod descriptor {
    pub const NUMBER: u32 = 1;
    pub const BOOLEAN: u32 = 2;
    pub const STRING: u32 = 3;
    pub const RECORD: u32 = 4;
    pub const ARRAY: u32 = 5;
    pub const EXCEPTION: u32 = 6;
    pub const SIGNED_INT: u32 = 8;
    pub const UNSIGNED_INT: u32 = 9;
    pub const JSON_VALUE: u32 = 10;
    pub const JSON_NUMERIC: u32 = 11;
    pub const JSON_TUPLE: u32 = 12;
    pub const JSON_VARIANT: u32 = 13;
    pub const JSON_DICTIONARY: u32 = 15;
    pub const JSON_DATE: u32 = 16;
    pub const JSON_BIGINT: u32 = 17;
    pub const JSON_VALUE_UNION: u32 = 18;
    pub const UNDEFINED_ALLOWED: u32 = 1;
    pub const NULL_ALLOWED: u32 = 2;
    pub const ABSENCE_MASK: u32 = UNDEFINED_ALLOWED | NULL_ALLOWED;
    pub const SCHEMA_MASK: i32 = !(ABSENCE_MASK as i32);
    pub const UNDEFINED_BITS: u64 = 0x7ff0_0000_0000_0001;
    pub const OMITTED_BITS: u64 = 0x7ff0_0000_0000_0003;
    pub const UNDEFINED_BOOL: i32 = 2;
    pub const OMITTED_BOOL: i32 = 4;
    pub const OMITTED_REF: i32 = 2;
    pub const COPY_UNDEFINED: i32 = -1;
    pub const COPY_ABSENT: i32 = -2;
    pub const COPY_NONFINITE: i32 = -3;
    pub const VARIANT_TAG: i32 = -4;

    pub const FIELDS: u32 = 8;
    pub const FIELD_SIZE: u32 = 12;
    pub const FIELD_SCHEMA: u32 = 4;
    pub const FIELD_OFFSET: u32 = 8;
    pub const DICTIONARY_VALUE_SCHEMA: u32 = FIELDS + FIELD_SIZE + FIELD_SCHEMA;
    pub const DICTIONARY_VALUE_OFFSET: u32 = FIELDS + FIELD_SIZE + FIELD_OFFSET;

    pub const WRITE_NULL: u32 = 0;
    pub const WRITE_TRUE: u32 = 4;
    pub const WRITE_FALSE: u32 = 8;
    pub const WRITE_EMPTY_OBJECT: u32 = 13;
    pub const WRITE_COLON: u32 = 15;
    pub const WRITE_QUOTE: u32 = 16;
    pub const WRITE_TAG_KEY: u32 = 20;
    pub const WRITE_VALUE_KEY: u32 = 24;
    pub const WRITE_TAG_NAME: u32 = 28;
    pub const WRITE_VALUE_NAME: u32 = 32;
    pub const WRITE_RETAINS_JSON: u32 = 36;
    pub const WRITE_DATE_BUFFER: u32 = 40;
    pub const WRITE_DATE_CAPACITY: u32 = 32;
    pub const WRITE_SIZE: u32 = WRITE_DATE_BUFFER + WRITE_DATE_CAPACITY;

    const _: () = {
        assert!(OMITTED_BITS == UNDEFINED_BITS + 2);
        assert!(OMITTED_BOOL == UNDEFINED_BOOL + 2 && OMITTED_REF == 2);
        assert!(super::numeric::NULL == super::numeric::UNDEFINED + 1);
        assert!(super::numeric::OMITTED == super::numeric::UNDEFINED + 2);
    };
}

#[path = "layout/regex.rs"]
pub mod regex;

pub mod numeric {
    macro_rules! tags {
        ($($name:ident = $value:literal as $raw:ident),+ $(,)?) => {
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #[repr(i32)]
            pub enum Tag { $($name = $value),+ }
            $(pub const $raw: i32 = Tag::$name as i32;)+
            impl TryFrom<i32> for Tag {
                type Error = i32;
                fn try_from(tag: i32) -> Result<Self, i32> {
                    match tag { $($value => Ok(Self::$name),)+ other => Err(other) }
                }
            }
        }
    }
    tags! {
        Float = 0 as FLOAT, Signed = 1 as SIGNED, Unsigned = 2 as UNSIGNED,
        Undefined = 3 as UNDEFINED, Null = 4 as NULL, Omitted = 5 as OMITTED,
        U8 = 6 as U8, S8 = 7 as S8, U16 = 8 as U16, S16 = 9 as S16,
        U32 = 10 as U32, S32 = 11 as S32, U64 = 12 as U64, S64 = 13 as S64,
    }

    impl Tag {
        pub fn fixed(self) -> Option<Integer> {
            ((self as i32) >= U8).then_some(Integer(self as u8))
        }

        pub fn integer_signed(self) -> Option<bool> {
            match self {
                Self::Float | Self::Null | Self::Undefined | Self::Omitted => None,
                _ => Some(self as i32 & 1 != 0),
            }
        }
    }

    /// The eight fixed-width integer carriers, in ABI tag order.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct Integer(u8);

    impl Integer {
        pub const U8: Self = Self(U8 as u8);
        pub const I8: Self = Self(S8 as u8);
        pub const U32: Self = Self(U32 as u8);
        pub const I32: Self = Self(S32 as u8);
        pub const U64: Self = Self(U64 as u8);
        pub const I64: Self = Self(S64 as u8);

        pub fn new(bits: u32, signed: bool) -> Option<Self> {
            let unsigned = match bits {
                8 => U8,
                16 => U16,
                32 => U32,
                64 => U64,
                _ => return None,
            };
            Some(Self(unsigned as u8 + u8::from(signed)))
        }

        pub fn tag(self) -> i32 {
            i32::from(self.0)
        }
        pub fn bits(self) -> u32 {
            8 << ((self.0 - U8 as u8) / 2)
        }
        pub fn signed(self) -> bool {
            self.0 & 1 != 0
        }

        pub fn common(self, other: Self) -> Self {
            let wider = self.max(other);
            if self.signed() == other.signed() {
                return wider;
            }
            let unsigned = if self.signed() { other } else { self };
            if unsigned == Self::U64 {
                return unsigned;
            }
            // The next wider signed tag follows the unsigned tag by three.
            wider.max(Self(unsigned.0 + 3))
        }

        pub fn register(self) -> Self {
            self.max(if self.signed() { Self::I32 } else { Self::U32 })
        }

        pub fn unconstrained(self) -> Self {
            if self.signed() { Self::I64 } else { Self::U64 }
        }
    }
}
