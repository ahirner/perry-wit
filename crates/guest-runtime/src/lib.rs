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

mod buffer;
mod cabi;
mod callbacks;
mod clocks;
mod date;
mod dispatch;
mod dispatch_pure;
mod environment;
mod equality;
mod filesystem;
mod headers;
mod http;
mod http_options;
mod http_url;
mod io;
mod nanbox;
mod objects;
mod random;
mod state;
mod stubs;
mod timers;

pub use cabi::{
    cabi_check_exception, cabi_debug_state, cabi_export_json, cabi_export_result_string,
    cabi_export_string, cabi_import_json, cabi_import_string, cabi_post_cleanup,
    cabi_post_result_cleanup, cabi_realloc, cabi_reclaim_callback_temporaries,
    cabi_reclaim_temporaries, cabi_record_init_checkpoint, cabi_register_global_root,
    cabi_reset_invocation_state,
};
pub use date::{
    date_get_date, date_get_day, date_get_full_year, date_get_hours, date_get_milliseconds,
    date_get_minutes, date_get_month, date_get_seconds, date_get_time, date_new_val, date_now,
    date_to_iso_string, performance_now,
};
pub use dispatch::{
    console_error, console_log, mem_call, mem_call_all_sync, mem_call_clocks, mem_call_clocks_env,
    mem_call_clocks_env_fs, mem_call_clocks_fs, mem_call_clocks_random, mem_call_clocks_random_env,
    mem_call_clocks_random_fs, mem_call_env, mem_call_env_fs, mem_call_fs, mem_call_i32,
    mem_call_random, mem_call_random_env, mem_call_random_env_fs, mem_call_random_fs, string_new,
};
pub use dispatch_pure::mem_call_pure;
