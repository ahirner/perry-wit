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

mod cabi;
mod dispatch;
mod equality;
mod http;
mod io;
mod nanbox;
mod state;
mod stubs;

pub use cabi::{
    cabi_export_json, cabi_export_result_string, cabi_export_string, cabi_import_json,
    cabi_import_string, cabi_post_cleanup, cabi_post_result_cleanup,
};
pub use dispatch::{console_error, console_log, mem_call, mem_call_i32, string_new};
