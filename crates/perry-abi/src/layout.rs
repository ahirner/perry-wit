//! Wasm32 aggregate layouts shared by generated code and linked helpers.
pub mod record {
    pub const SHAPE: u32 = 0;
    pub const JSON: u32 = 4;
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
    pub const SLOT: u32 = 8;
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
