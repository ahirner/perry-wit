//! Scheduling cost with independent tasks and no external I/O latency.

use anyhow::{Context, Result, ensure};
use perry_wit::{CompileOptions, compile_typescript};
use serde::Serialize;
use std::{fs, time::Instant};
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, ResourceLimiter, Store};

#[derive(Default)]
struct MemoryUsage(usize);
impl ResourceLimiter for MemoryUsage {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        wasmtime::ensure!(
            desired <= 8 * 1024 * 1024 && maximum.is_none_or(|limit| desired <= limit),
            "measurement memory limit"
        );
        self.0 += desired - current;
        Ok(true)
    }
    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        Err(error)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

#[derive(Serialize)]
struct Measurement {
    operands: u32,
    component_bytes: usize,
    memory_after_warmup: usize,
    memory_after_samples: usize,
    calls_per_sample: u32,
    microseconds_per_call: Vec<f64>,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "set PERRY_ASYNC_MEASUREMENT_OUTPUT to record local scheduling measurements"]
async fn measure_owned_fanout() -> Result<()> {
    let output = std::env::var_os("PERRY_ASYNC_MEASUREMENT_OUTPUT")
        .context("set PERRY_ASYNC_MEASUREMENT_OUTPUT")?;
    let root = tempfile::tempdir()?;
    fs::write(
        root.path().join("world.wit"),
        "package test:scheduling; world task {export run:async func(count:u32)->f64;}",
    )?;
    let compiled = compile_typescript(
        r#"
      async function task(value:number):Promise<number> {await 0; return value;}
      export async function run(count:number):Promise<number> {
        const tasks:Promise<number>[]=[];
        for(let i=0;i<count;i=i+1)tasks.push(task(i));
        const values=await Promise.all(tasks);
        let total=0;
        for(let i=0;i<values.length;i=i+1)total=total+values[i];
        return total;
      }
    "#,
        "fanout.ts",
        &CompileOptions {
            wit_dir: root.path().into(),
            world: Some("task".into()),
            core_only: false,
        },
    )?;
    let bytes = compiled.stripped.unwrap();
    let component_bytes = bytes.len();
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, bytes)?;
    let mut measurements = Vec::new();
    for operands in [1u32, 16, 64, 256] {
        let mut store = Store::new(&engine, MemoryUsage::default());
        store.limiter(|memory| memory);
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance.get_typed_func::<(u32,), (f64,)>(&mut store, "run")?;
        let expected = f64::from(operands * (operands - 1) / 2);
        for _ in 0..5 {
            assert_eq!(run.call_async(&mut store, (operands,)).await?.0, expected);
        }
        let memory_after_warmup = store.data().0;
        let calls_per_sample = 50;
        let mut microseconds_per_call = Vec::new();
        for _ in 0..5 {
            let started = Instant::now();
            for _ in 0..calls_per_sample {
                assert_eq!(run.call_async(&mut store, (operands,)).await?.0, expected);
            }
            microseconds_per_call
                .push(started.elapsed().as_secs_f64() * 1e6 / f64::from(calls_per_sample));
        }
        store.assert_concurrent_state_empty();
        ensure!(
            memory_after_warmup == store.data().0,
            "serial calls keep growing memory"
        );
        let sample = Measurement {
            operands,
            component_bytes,
            memory_after_warmup,
            memory_after_samples: store.data().0,
            calls_per_sample,
            microseconds_per_call,
        };
        eprintln!("{}", serde_json::to_string(&sample)?);
        measurements.push(sample);
    }
    fs::write(output, serde_json::to_string_pretty(&measurements)?)?;
    Ok(())
}
