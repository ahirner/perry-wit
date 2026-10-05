#![no_std]

/// Floating-point remainder uses the compiler's fmod implementation, preserving
/// dividend-signed zero and avoiding rounded quotient/product cancellation.
#[unsafe(no_mangle)]
pub extern "C" fn number_remainder(dividend: f64, divisor: f64) -> f64 {
    dividend % divisor
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo<'_>) -> ! {
    core::arch::wasm32::unreachable()
}
