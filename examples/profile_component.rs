//! Phase timings and sampled guest stacks for a JSON-described component invocation.
//! Run with `cargo run --release --example profile_component -- request.json`.
//! Compilation is separate; each measured call gets a fresh instance and WASI context.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wasmtime::{
    Config, Engine, Store, UpdateDeadline, WasmBacktrace,
    component::{Component, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    component: PathBuf,
    export: Vec<String>,
    arguments: Vec<Argument>,
    environment: BTreeMap<String, String>,
    documents: PathBuf,
    samples: usize,
    #[serde(default)]
    profile: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Argument {
    String(String),
    Enum(String),
    U32(u32),
}

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    stacks: BTreeMap<String, usize>,
    linear_bytes: usize,
}

impl wasmtime::ResourceLimiter for Host {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.linear_bytes += desired - current;
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

fn milliseconds(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let request: Request = serde_json::from_slice(&fs::read(
        std::env::args_os()
            .nth(1)
            .context("request.json required")?,
    )?)?;
    ensure!(
        request.samples > 0 && !request.export.is_empty(),
        "samples and export must be nonempty"
    );
    let start = Instant::now();
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .epoch_interruption(request.profile);
    let engine = Engine::new(&config)?;
    let engine_ms = milliseconds(start);
    let start = Instant::now();
    let component = Component::from_file(&engine, &request.component)?;
    let compile_ms = milliseconds(start);
    let artifact = tempfile::NamedTempFile::new()?;
    fs::write(artifact.path(), component.serialize()?)?;
    drop(component);
    let stop = Arc::new(AtomicBool::new(false));
    let ticker = request.profile.then(|| {
        let engine = engine.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_micros(100));
                engine.increment_epoch();
            }
        })
    });
    let arguments: Vec<_> = request
        .arguments
        .iter()
        .map(|argument| match argument {
            Argument::String(value) => Val::String(value.clone()),
            Argument::Enum(value) => Val::Enum(value.clone()),
            Argument::U32(value) => Val::U32(*value),
        })
        .collect();
    let mut measurements = Vec::new();
    let mut stacks = BTreeMap::<String, usize>::new();
    let mut output = None;
    for _ in 0..request.samples {
        let start = Instant::now();
        // SAFETY: this process just compiled and serialized this private temporary file.
        let component = unsafe { Component::deserialize_file(&engine, artifact.path())? };
        let load_ms = milliseconds(start);
        let start = Instant::now();
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        let pre = linker.instantiate_pre(&component)?;
        let mut ctx = WasiCtx::builder();
        ctx.envs(&request.environment.iter().collect::<Vec<_>>());
        ctx.preopened_dir(&request.documents, "/documents", FsPerms::ReadOnly)?;
        let mut store = Store::new(
            &engine,
            Host {
                ctx: ctx.build(),
                table: ResourceTable::new(),
                stacks: BTreeMap::new(),
                linear_bytes: 0,
            },
        );
        store.limiter(|host| host);
        if request.profile {
            store.set_epoch_deadline(1);
            store.epoch_deadline_callback(|mut context| {
                let frames = WasmBacktrace::force_capture(&context);
                let stack = frames
                    .frames()
                    .iter()
                    .map(|frame| {
                        frame
                            .func_name()
                            .map(str::to_owned)
                            .unwrap_or_else(|| format!("wasm-function[{}]", frame.func_index()))
                    })
                    .collect::<Vec<_>>()
                    .join(";");
                *context.data_mut().stacks.entry(stack).or_default() += 1;
                Ok(UpdateDeadline::Continue(1))
            });
        }
        let setup_ms = milliseconds(start);
        let start = Instant::now();
        let instance = pre.instantiate_async(&mut store).await?;
        let instantiate_ms = milliseconds(start);
        let mut index = None;
        for name in &request.export {
            index = Some(
                instance
                    .get_export(&mut store, index.as_ref(), name)
                    .with_context(|| format!("missing export {name}"))?
                    .1,
            );
        }
        let function = instance
            .get_func(&mut store, index.unwrap())
            .context("export is not a function")?;
        let mut results = [Val::Bool(false)];
        let start = Instant::now();
        function
            .call_async(&mut store, &arguments, &mut results)
            .await?;
        let call_ms = milliseconds(start);
        let Val::String(value) = results.into_iter().next().unwrap() else {
            anyhow::bail!("expected string result")
        };
        if let Some(previous) = &output {
            ensure!(*previous == value, "results changed across fresh instances");
        }
        output = Some(value);
        for (stack, count) in std::mem::take(&mut store.data_mut().stacks) {
            *stacks.entry(stack).or_default() += count;
        }
        let linear_bytes = store.data().linear_bytes;
        let start = Instant::now();
        drop((store, pre, linker, component));
        let teardown_ms = milliseconds(start);
        measurements.push(serde_json::json!({"loadMs":load_ms,"setupMs":setup_ms,"instantiateMs":instantiate_ms,"callMs":call_ms,"teardownMs":teardown_ms,"linearMemoryBytes":linear_bytes}));
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(ticker) = ticker {
        ticker.join().expect("epoch ticker");
    }
    println!(
        "{}",
        serde_json::json!({"engineMs":engine_ms,"compileMs":compile_ms,"samples":measurements,"stacks":stacks,"output":output,"wasmtimeVersion":"49.0.2","instrumented":request.profile})
    );
    Ok(())
}
