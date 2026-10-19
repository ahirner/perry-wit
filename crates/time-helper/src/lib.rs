#![cfg_attr(target_arch = "wasm32", no_std)]
#![cfg(any(target_arch = "wasm32", test))]
#![cfg_attr(test, allow(dead_code, unused_imports))]

mod time;
pub(crate) use time::*;
mod date;
pub(crate) use date::{date_part, date_set, date_utc, parse_date_milliseconds};

#[cfg(target_arch = "wasm32")]
mod guest;
#[cfg(test)]
mod tests;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
