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
#[path = "../support/node_oracle.rs"]
mod node_oracle;
#[path = "../support/output_capture.rs"]
mod output_capture;
#[path = "../support/wasi_host.rs"]
mod wasi_host;
#[path = "../support/world.rs"]
mod world;

mod async_component_test;
mod byob_test;
mod cancellation_test;
mod class_test;
mod collection_test;
mod command_test;
mod component_budget_test;
mod component_test;
mod content_api_test;
mod diagnostics_test;
mod examples_test;
mod fetch_test;
mod import_test;
mod io_cancellation_test;
mod json_read_test;
mod mir_collection_test;
mod mir_string_test;
mod object_test;
mod optimization_test;
mod resource_test;
mod runtime_error_test;
mod sdk_test;
mod stream_test;
mod wit_resolution_test;
