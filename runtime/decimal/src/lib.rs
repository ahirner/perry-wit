#![no_std]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;
use perry_rt_sys::{
    number::{DecimalSyntax, ascii_digit, scan_decimal, split_sign},
    whitespace::{trim_bytes, trim_start_bytes},
};

/// ECMAScript `parseFloat(string)`.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_parse_float(string: *mut u8) -> f64 {
    let trimmed = trim_start_bytes(unsafe { perry_rt_sys::bytes(string) });
    let (negative, rest) = split_sign(trimmed);
    if rest.starts_with(b"Infinity") {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let Ok(literal) = scan_decimal::<false>(rest, DecimalSyntax::Prefix) else {
        return f64::NAN;
    };
    literal.float(negative).unwrap_or(f64::NAN)
}

/// ECMAScript `StringToNumber` (`Number(string)`).
#[unsafe(no_mangle)]
pub(crate) extern "C" fn std_string_to_number(string: *mut u8) -> f64 {
    let trimmed = trim_bytes(unsafe { perry_rt_sys::bytes(string) });
    if trimmed.is_empty() {
        return 0.0;
    }
    if trimmed.len() > 2 && trimmed[0] == b'0' {
        let radix = match trimmed[1] {
            b'x' | b'X' => Some(16u32),
            b'o' | b'O' => Some(8u32),
            b'b' | b'B' => Some(2u32),
            _ => None,
        };
        if let Some(radix) = radix {
            let mut value = 0.0f64;
            for &byte in &trimmed[2..] {
                let Some(digit) = ascii_digit(byte, radix) else {
                    return f64::NAN;
                };
                value = value * f64::from(radix) + f64::from(digit);
            }
            return value;
        }
    }
    let (negative, rest) = split_sign(trimmed);
    if rest == b"Infinity" {
        return if negative {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let Ok(literal) = scan_decimal::<false>(rest, DecimalSyntax::Prefix) else {
        return f64::NAN;
    };
    if literal.consumed != rest.len() {
        return f64::NAN;
    }
    literal.float(negative).unwrap_or(f64::NAN)
}
