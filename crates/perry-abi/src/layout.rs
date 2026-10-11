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
    pub const SOURCE: u32 = 16;
    pub const SIZE: u32 = 24;
}
