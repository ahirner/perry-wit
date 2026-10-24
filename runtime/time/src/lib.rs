#![no_std]
#![cfg(any(target_arch = "wasm32", test))]
#![cfg_attr(test, allow(dead_code, unused_imports))]

#[cfg(not(target_arch = "wasm32"))]
extern crate std;

use perry_rt_sys as _;

mod time;
pub(crate) use time::*;
mod date;
pub(crate) use date::{date_part, date_set, date_utc, parse_date_milliseconds};

#[cfg(target_arch = "wasm32")]
mod guest;
#[cfg(test)]
mod tests;
