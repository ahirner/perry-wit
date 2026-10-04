//! Opt-in size, memory, and throughput measurements of production components.

use anyhow::{Context, Result, ensure};
use perry_wit::{CompileOptions, compile_file};
use serde::Serialize;
use std::{fs, path::Path, time::Instant};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[derive(Default)]
struct MemoryUsage {
    allocated: usize,
}
impl ResourceLimiter for MemoryUsage {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        if maximum.is_some_and(|limit| desired > limit) || desired > 256 * 1024 * 1024 {
            return Err(wasmtime::Error::msg("measurement exceeds memory limit"));
        }
        self.allocated += desired - current;
        Ok(true)
    }
    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        Err(error)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}
struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    memory: MemoryUsage,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[derive(Serialize)]
struct Measurement {
    pipeline: &'static str,
    workload: &'static str,
    component_bytes: usize,
    memory_after_warmup: usize,
    memory_after_samples: usize,
    calls_per_sample: usize,
    payload_bytes: usize,
    milliseconds_per_call: Vec<f64>,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "set PERRY_MEASUREMENT_OUTPUT to record size, memory, and throughput"]
async fn measure_components() -> Result<()> {
    let output = std::env::var_os("PERRY_MEASUREMENT_OUTPUT")
        .context("set PERRY_MEASUREMENT_OUTPUT to a JSON output path")?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_async_stackful(true)
        .wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let directory = tempfile::tempdir()?;
    let text_input = "abcdefghijklmnopqrstuvwxyz0123456789".repeat(128)[..4096].to_owned();
    let text_expected = format!("{text_input}{}", "!".repeat(64));
    let file_data = "abcdefgh01234567".repeat(4096);
    fs::write(directory.path().join("input"), &file_data)?;
    let mut measurements = Vec::new();
    for (workload, input, expected, payload_bytes) in [
        (
            "text",
            text_input.as_str(),
            text_expected.as_str(),
            text_input.len(),
        ),
        ("io", "/sandbox/input", file_data.as_str(), file_data.len()),
    ] {
        let source = Path::new("tests/fixtures/measurement").join(format!("{workload}.ts"));
        let pipeline = "production";
        let version = "0.3.0";
        let wit = directory.path().join(format!("wit-{pipeline}"));
        fs::create_dir_all(&wit)?;
        let async_export = if workload == "io" { "async " } else { "" };
        fs::write(
            wit.join("world.wit"),
            format!(
                "package test:measurement; world task {{ include wasi:cli/imports@{version}; export run-task: {async_export}func(input: string) -> string; }}"
            ),
        )?;
        let bytes = compile_file(
            &source,
            &CompileOptions {
                wit_dir: wit,
                world: Some("task".into()),
                core_only: false,
            },
        )?
        .stripped
        .unwrap();
        let component_bytes = bytes.len();
        let component = Component::new(&engine, bytes)?;
        let mut linker = Linker::<Host>::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        let mut store = Store::new(
            &engine,
            Host {
                wasi: WasiCtxBuilder::new()
                    .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
                    .build(),
                table: ResourceTable::new(),
                memory: MemoryUsage::default(),
            },
        );
        store.limiter(|host| &mut host.memory);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
        for _ in 0..5 {
            assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
        }
        let memory_after_warmup = store.data().memory.allocated;
        let calls_per_sample = 50;
        let mut milliseconds_per_call = Vec::new();
        for _ in 0..5 {
            let start = Instant::now();
            for _ in 0..calls_per_sample {
                assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
            }
            milliseconds_per_call
                .push(start.elapsed().as_secs_f64() * 1000.0 / calls_per_sample as f64);
        }
        store.assert_concurrent_state_empty();
        ensure!(
            store.data().table.is_empty(),
            "resources remain after {pipeline} {workload}"
        );
        if workload == "io" {
            assert_eq!(
                fs::read_to_string(directory.path().join("input.copy"))?,
                file_data
            );
        }
        let result = Measurement {
            pipeline,
            workload,
            component_bytes,
            memory_after_warmup,
            memory_after_samples: store.data().memory.allocated,
            calls_per_sample,
            payload_bytes,
            milliseconds_per_call,
        };
        eprintln!("{}", serde_json::to_string(&result)?);
        measurements.push(result);
    }
    fs::write(output, serde_json::to_string_pretty(&measurements)?)?;
    Ok(())
}
