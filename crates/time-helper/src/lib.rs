#![cfg_attr(target_arch = "wasm32", no_std)]

mod time;
pub use time::*;
mod date;
pub use date::parse_date_milliseconds;

#[cfg(target_arch = "wasm32")]
mod guest;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
