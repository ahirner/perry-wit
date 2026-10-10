//! Layout of a compiled host-call transfer. Result areas and captured argument owners stay
//! reachable until the host completes or acknowledges cancellation.
pub const AREA: u32 = 0;
pub const OWNERS: u32 = 4;
pub const KIND: u32 = 8;
pub const RETURNED: u32 = 12;
pub const SCOPE: u32 = 16;
pub const SIZE: u32 = 20;
