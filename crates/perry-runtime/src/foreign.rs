//! Layout of a compiled host-call transfer. Result areas and captured argument owners stay
//! reachable until the host completes or acknowledges cancellation.
pub use crate::layout::foreign::{AREA, KIND, OWNERS, RETURNED, SCOPE, SIZE};
