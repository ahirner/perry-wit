//! Minimal guest runtime for WASI Preview 2 components compiled by Perry.
#![warn(unreachable_pub)]

#[allow(warnings)]
mod bindings {
    wit_bindgen::generate!({
        path: "../../wit",
        world: "runtime-adapter",
        generate_all,
    });
}

mod dispatch;
mod http;
mod io;
mod nanbox;
mod state;
mod stubs;

pub use dispatch::{console_error, console_log, mem_call, mem_call_i32, string_new};
