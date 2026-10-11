//! Wasm32 aggregate layouts shared by generated code and linked helpers.
pub mod record {
    pub const SHAPE: u32 = 0;
    pub const SOURCE: u32 = 4;
    pub const HEADER: u32 = 8;
    pub const SLOT: u32 = 8;

    pub const fn field(index: usize) -> u32 {
        HEADER + SLOT * index as u32
    }
}

pub mod array {
    pub const LEN: u32 = 0;
    pub const CAP: u32 = 4;
    pub const DATA: u32 = 8;
    pub const SOURCE: u32 = 12;
    pub const SLOT: u32 = 8;
}

pub mod variant {
    pub const TAG: u32 = 0;
    pub const OWNER: u32 = 4;
    pub const VALUE: u32 = 8;
    pub const BASE_SIZE: u32 = 16;
    pub const SOURCE: u32 = 16;
    pub const SIZE: u32 = 24;
}

/// Native helpers and Wasm code share this definition, including pooled string headers.
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

/// Aggregate codes passed by typed JSON decoders when attaching source provenance.
pub mod json_source {
    pub const RECORD: u32 = 0;
    pub const ARRAY: u32 = 1;
    pub const VARIANT: u32 = 2;
    pub const TUPLE: u32 = 3;

    pub const fn offset(kind: u32) -> u32 {
        match kind {
            RECORD | TUPLE => super::record::SOURCE,
            ARRAY => super::array::SOURCE,
            VARIANT => super::variant::SOURCE,
            _ => panic!("invalid JSON aggregate layout"),
        }
    }
}
