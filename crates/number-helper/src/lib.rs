#![cfg_attr(target_arch = "wasm32", no_std)]

#[path = "../../../src/helpers/number.rs"]
mod implementation;

pub use implementation::*;

#[cfg(target_arch = "wasm32")]
#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
