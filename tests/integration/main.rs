//! Functional integration tests share one executable. Compilation timing, benchmarks,
//! and cold packaging retain separate entry points.

#[path = "../support/cancellation_caller.rs"]
mod cancellation;
#[path = "../support/core_program.rs"]
mod core_program;
#[path = "../support/engine.rs"]
mod engine;
#[allow(dead_code)]
#[path = "../support/http_fixture.rs"]
mod http_fixture;
#[allow(dead_code, unused_imports)]
#[path = "../../src/link/mod.rs"]
mod link;
#[path = "../support/output_capture.rs"]
mod output_capture;
#[path = "../support/wasi_host.rs"]
mod wasi_host;
#[path = "../support/world.rs"]
mod world;

mod async_component_test;
mod byob_test;
mod cancellation_test;
mod command_test;
mod component_budget_test;
mod component_test;
mod content_api_test;
mod decoder_test;
mod diagnostics_test;
mod examples_test;
mod fetch_cancellation_test;
mod fetch_test;
mod fetch_url_test;
mod historical_error_test;
mod http_handler_test;
mod import_test;
mod io_cancellation_test;
mod json_helper_test;
mod json_read_test;
mod memory_growth_test;
mod mir_codegen_test;
mod mir_collection_test;
mod mir_string_test;
mod module_test;
mod optimization_test;
mod resource_test;
mod runtime_error_test;
mod sdk_test;
mod stream_test;
mod time_helper_test;
mod url_test;
mod wit_resolution_test;
