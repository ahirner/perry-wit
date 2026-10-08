//! Opt-in size, memory, and throughput measurements of production components.

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use perry_wit::{CompileOptions, compile_file};
use serde::Serialize;
use std::{fs, future::Future, path::Path, time::Instant};
use wasmtime_wasi_http::{WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};
#[path = "support/call_counts.rs"]
mod call_counts;
#[path = "support/waffle.rs"]
mod waffle_fixture;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, ResourceLimiter, Store};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

struct MemoryUsage {
    allocated: usize,
    limit: usize,
}
impl Default for MemoryUsage {
    fn default() -> Self {
        Self {
            allocated: 0,
            limit: 256 * 1024 * 1024,
        }
    }
}
impl ResourceLimiter for MemoryUsage {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let allocated = self.allocated + desired - current;
        if maximum.is_some_and(|limit| desired > limit) || allocated > self.limit {
            return Err(wasmtime::Error::msg("measurement exceeds memory limit"));
        }
        self.allocated = allocated;
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
    http: WasiHttpCtx,
    response: ResponseFixture,
}
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[derive(Default)]
struct ResponseFixture {
    body: Bytes,
    requests: usize,
}
impl WasiHttpHooks for ResponseFixture {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _: Option<wasmtime_wasi_http::RequestOptions>,
        _: Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), wasmtime_wasi_http::Error>> + Send>,
                    ),
                    wasmtime_wasi_http::Error,
                >,
            > + Send,
    > {
        assert_eq!(request.method(), "GET");
        assert_eq!(request.uri().to_string(), "http://measurement.invalid/");
        self.requests += 1;
        let body = Full::new(self.body.clone())
            .map_err(|never| match never {})
            .boxed_unsync();
        Box::new(async move {
            request.into_body().collect().await?;
            Ok((http::Response::new(body), Box::new(async { Ok(()) }) as _))
        })
    }
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.response,
        }
    }
}

#[derive(Serialize)]
struct SizeMeasurement {
    workload: &'static str,
    component_bytes: usize,
}
#[derive(Serialize)]
struct Report {
    placement: String,
    timings: Vec<Measurement>,
    sizes: Vec<SizeMeasurement>,
}
#[derive(Serialize)]
struct Measurement {
    workload: &'static str,
    component_bytes: usize,
    memory_limit_bytes: usize,
    memory_after_warmup: usize,
    memory_after_samples: usize,
    warmup_calls: usize,
    calls_per_sample: usize,
    payload_bytes: usize,
    milliseconds_per_call: Vec<f64>,
    counts: Option<call_counts::Counts>,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "set PERRY_MEASUREMENT_OUTPUT to record size, memory, and throughput"]
async fn measure_components() -> Result<()> {
    let placement =
        std::env::var("PERRY_MEASUREMENT_PLACEMENT").unwrap_or_else(|_| "root-future".to_owned());
    match placement.as_str() {
        "root-future" => measure_campaign(placement).await,
        "spawned-task" => tokio::spawn(measure_campaign(placement)).await?,
        _ => anyhow::bail!("unknown measurement placement: {placement}"),
    }
}

async fn measure_campaign(placement: String) -> Result<()> {
    let output = std::env::var_os("PERRY_MEASUREMENT_OUTPUT")
        .context("set PERRY_MEASUREMENT_OUTPUT to a JSON output path")?;
    let output = std::path::PathBuf::from(output);
    let artifacts = output.parent().context("measurement output directory")?;
    fs::create_dir_all(artifacts)?;
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
        let wit = directory.path().join("wit");
        fs::create_dir_all(&wit)?;
        let async_export = if workload == "io" { "async " } else { "" };
        fs::write(
            wit.join("world.wit"),
            format!(
                "package test:measurement; world task {{ include wasi:cli/imports@0.3.0; export run-task: {async_export}func(input: string) -> string; }}"
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
                http: WasiHttpCtx::new(),
                response: ResponseFixture::default(),
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
            "resources remain after {workload}"
        );
        if workload == "io" {
            assert_eq!(
                fs::read_to_string(directory.path().join("input.copy"))?,
                file_data
            );
        }
        let result = Measurement {
            workload,
            component_bytes,
            memory_limit_bytes: store.data().memory.limit,
            memory_after_warmup,
            memory_after_samples: store.data().memory.allocated,
            warmup_calls: 5,
            calls_per_sample,
            payload_bytes,
            milliseconds_per_call,
            counts: None,
        };
        eprintln!("{}", serde_json::to_string(&result)?);
        measurements.push(result);
    }
    for byob in [false, true] {
        measurements.push(measure_stream(&engine, artifacts, byob).await?);
    }
    for reader in [HttpRead::Default, HttpRead::Byob, HttpRead::ArrayBuffer] {
        measurements.extend(measure_http(&engine, artifacts, reader).await?);
    }

    let mut sizes = Vec::new();
    for (workload, source, wit, world) in [
        (
            "merge_docs.ts",
            "examples/merge_docs.ts",
            "wit",
            "merge-docs",
        ),
        (
            "merge_task.ts",
            "examples/merge_task.ts",
            "wit",
            "task-runner",
        ),
        (
            "template-task",
            "template/src/index.ts",
            "template/wit",
            "task",
        ),
    ] {
        let bytes = compile_file(
            Path::new(source),
            &CompileOptions {
                wit_dir: wit.into(),
                world: Some(world.into()),
                core_only: false,
            },
        )?
        .stripped
        .context("stripped example component")?;
        sizes.push(SizeMeasurement {
            workload,
            component_bytes: bytes.len(),
        });
        fs::write(artifacts.join(format!("{workload}.wasm")), bytes)?;
    }
    fs::write(
        output,
        serde_json::to_string_pretty(&Report {
            placement,
            timings: measurements,
            sizes,
        })?,
    )?;
    Ok(())
}

const PAYLOAD_BYTES: usize = 4 * 1024 * 1024;
const WARMUP_CALLS: usize = 5;
const SAMPLE_COUNT: usize = 5;
const CALLS_PER_SAMPLE: usize = 5;

const STREAM: &str = r#"
export async function run(input: ReadableStream<Uint8Array>): Promise<number> {
    const reader=input.getReader();
    let total=0;
    let result=await reader.read();
    while(!result.done) {
        const bytes=result.value;if(bytes===undefined)throw 1;
        for(let index=0;index<bytes.length;index++)total+=bytes[index];
        result=await reader.read();
    }
    reader.releaseLock();
    return total;
}
"#;

const BYOB_STREAM: &str = r#"
export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
    const reader=input.getReader({mode:"byob"});
    let buffer=new Uint8Array(8192);let total=0;
    while(true) {
        const result=await reader.read(buffer);
        const bytes=result.value;if(bytes===undefined)throw 90;
        for(let index=0;index<bytes.length;index++)total+=bytes[index];
        if(result.done)break;
        buffer=new Uint8Array(bytes.buffer);
    }
    reader.releaseLock();return total;
}
"#;

async fn measure_stream(engine: &Engine, artifacts: &Path, byob: bool) -> Result<Measurement> {
    use wasmtime::component::StreamReader;
    let source = if byob { BYOB_STREAM } else { STREAM };
    let workload = if byob {
        "incoming-stream-byob"
    } else {
        "incoming-stream"
    };
    fs::write(artifacts.join(format!("{workload}.ts")), source)?;
    let compiled = waffle_fixture::compile_typescript_waffle(
        source,
        "incoming-stream.ts",
        &Default::default(),
    )?;
    let stream_bytes =
        perry_wit::strip::component(compiled.component.as_deref().context("stream component")?)?;
    fs::write(artifacts.join(format!("{workload}.wasm")), &stream_bytes)?;
    let stream_size = stream_bytes.len();
    let stream = Component::new(engine, stream_bytes)?;
    let mut store = Store::new(
        engine,
        MemoryUsage {
            allocated: 0,
            limit: 65536,
        },
    );
    store.limiter(|memory| memory);
    let instance = Linker::new(engine)
        .instantiate_async(&mut store, &stream)
        .await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    let payload = (0..PAYLOAD_BYTES)
        .map(|index| (index * 37 + index / 251) as u8)
        .collect::<Vec<_>>();
    let expected = payload.iter().map(|byte| f64::from(*byte)).sum::<f64>();
    for _ in 0..WARMUP_CALLS {
        let input = StreamReader::new(&mut store, payload.clone())?;
        ensure!(
            run.call_async(&mut store, (input,)).await?.0 == expected,
            "stream checksum differs"
        );
    }
    let memory_after_warmup = store.data().allocated;
    let mut milliseconds_per_call = Vec::new();
    for _ in 0..SAMPLE_COUNT {
        let mut elapsed = std::time::Duration::ZERO;
        for _ in 0..CALLS_PER_SAMPLE {
            let input = StreamReader::new(&mut store, payload.clone())?;
            let started = Instant::now();
            let (sum,) = run.call_async(&mut store, (input,)).await?;
            elapsed += started.elapsed();
            ensure!(sum == expected, "stream checksum differs");
            store.assert_concurrent_state_empty();
        }
        milliseconds_per_call.push(elapsed.as_secs_f64() * 1000.0 / CALLS_PER_SAMPLE as f64);
    }
    ensure!(
        store.data().allocated == memory_after_warmup,
        "stream memory grew after warmup"
    );
    let counters = call_counts::Counters::new(
        &compiled,
        "package test:measurement; world task {export run:async func(input:stream<u8>)->f64;}",
        artifacts,
        workload,
    )?;
    let component = Component::new(engine, &counters.component)?;
    let mut counted_store = Store::new(
        engine,
        MemoryUsage {
            allocated: 0,
            limit: 65536,
        },
    );
    counted_store.limiter(|memory| memory);
    let counted = Linker::new(engine)
        .instantiate_async(&mut counted_store, &component)
        .await?;
    let counted_run =
        counted.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut counted_store, "run")?;
    for _ in 0..WARMUP_CALLS {
        let input = StreamReader::new(&mut counted_store, payload.clone())?;
        ensure!(
            counted_run
                .call_async(&mut counted_store, (input,))
                .await?
                .0
                == expected,
            "instrumented stream checksum differs"
        );
    }
    let before = counters.snapshot(&mut counted_store, &counted).await?;
    let input = StreamReader::new(&mut counted_store, payload)?;
    let (result, polls) =
        call_counts::count_polls(counted_run.call_async(&mut counted_store, (input,))).await;
    ensure!(
        result?.0 == expected,
        "instrumented stream checksum differs"
    );
    counted_store.assert_concurrent_state_empty();
    let after = counters.snapshot(&mut counted_store, &counted).await?;
    Ok(Measurement {
        workload,
        component_bytes: stream_size,
        memory_limit_bytes: store.data().limit,
        memory_after_warmup,
        memory_after_samples: store.data().allocated,
        warmup_calls: WARMUP_CALLS,
        calls_per_sample: CALLS_PER_SAMPLE,
        payload_bytes: PAYLOAD_BYTES,
        milliseconds_per_call,
        counts: Some(counters.difference(&before, &after, polls)),
    })
}

enum HttpRead {
    Default,
    Byob,
    ArrayBuffer,
}

async fn measure_http(
    engine: &Engine,
    artifacts: &Path,
    reader: HttpRead,
) -> Result<Vec<Measurement>> {
    let read = match reader {
        HttpRead::Default | HttpRead::Byob => {
            "const response=await fetch('http://'+authority+'/');const body=await readBounded(response,limit);"
        }
        HttpRead::ArrayBuffer => {
            "const response=await fetch('http://'+authority+'/');const body=new Uint8Array(await response.arrayBuffer());"
        }
    };
    let prefix = match reader {
        HttpRead::Default => include_str!("fixtures/bounded_response.ts"),
        HttpRead::Byob => include_str!("fixtures/bounded_byob_response.ts"),
        HttpRead::ArrayBuffer => "",
    };
    let source = format!(
        r#"{prefix}
export async function run(authority:string,limit:number):Promise<number> {{
  try {{
    {read}
    if(response.status!==200||body[0]!==0||body[body.length-1]!==255)throw 91;
    return body.length;
  }} catch(error) {{if(error===8)return -8;throw error;}}
}}
"#
    );
    let name = match reader {
        HttpRead::Default => "bounded-http",
        HttpRead::Byob => "bounded-http-byob",
        HttpRead::ArrayBuffer => "unbounded-http-array-buffer",
    };
    fs::write(artifacts.join(format!("{name}.ts")), &source)?;
    let wit = tempfile::tempdir()?;
    let world_source = "package test:measurement; world task {import wasi:http/client@0.3.0; export run:async func(authority:string,limit:f64)->f64;}";
    fs::write(wit.path().join("world.wit"), world_source)?;
    let (resolve, package) = perry_wit::component::wit::resolve_wit(wit.path())?;
    let world = resolve.select_world(&[package], Some("task"))?;
    let compiled = perry_wit::waffle_backend::compile_typescript_for_world(
        &source,
        &format!("{name}.ts"),
        &Default::default(),
        resolve,
        world,
    )?;
    let bytes =
        perry_wit::strip::component(compiled.component.as_deref().context("HTTP component")?)?;
    let counters = call_counts::Counters::new(&compiled, world_source, artifacts, name)?;
    fs::write(artifacts.join(format!("{name}.wasm")), &bytes)?;
    let component_bytes = bytes.len();
    let component = Component::new(engine, bytes)?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = http_store(engine);
    let counted_component = Component::new(engine, &counters.component)?;
    let mut counted_store = http_store(engine);
    let counted = linker
        .instantiate_async(&mut counted_store, &counted_component)
        .await?;
    let counted_run = counted.get_typed_func::<(&str, f64), (f64,)>(&mut counted_store, "run")?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, f64), (f64,)>(&mut store, "run")?;
    let mut measurements = Vec::new();
    let outcomes: &[(&str, usize, f64)] = match reader {
        HttpRead::Default => &[
            ("bounded-http-exact", PAYLOAD_BYTES, PAYLOAD_BYTES as f64),
            ("bounded-http-overflow", PAYLOAD_BYTES - 1, -8.0),
        ],
        HttpRead::Byob => &[
            (
                "bounded-http-exact-byob",
                PAYLOAD_BYTES,
                PAYLOAD_BYTES as f64,
            ),
            ("bounded-http-overflow-byob", PAYLOAD_BYTES - 1, -8.0),
        ],
        HttpRead::ArrayBuffer => &[("unbounded-http-array-buffer", 0, PAYLOAD_BYTES as f64)],
    };
    for &(workload, limit, expected) in outcomes {
        for _ in 0..WARMUP_CALLS {
            ensure!(
                run.call_async(&mut store, ("measurement.invalid", limit as f64))
                    .await?
                    .0
                    == expected,
                "wrong HTTP outcome"
            );
        }
        let memory_after_warmup = store.data().memory.allocated;
        let mut milliseconds_per_call = Vec::new();
        for _ in 0..SAMPLE_COUNT {
            let mut elapsed = std::time::Duration::ZERO;
            for _ in 0..CALLS_PER_SAMPLE {
                let started = Instant::now();
                let (result,) = run
                    .call_async(&mut store, ("measurement.invalid", limit as f64))
                    .await?;
                elapsed += started.elapsed();
                ensure!(result == expected, "wrong HTTP outcome");
                store.assert_concurrent_state_empty();
                ensure!(store.data().table.is_empty(), "HTTP resources remain");
            }
            milliseconds_per_call.push(elapsed.as_secs_f64() * 1000.0 / CALLS_PER_SAMPLE as f64);
        }
        ensure!(
            store.data().memory.allocated == memory_after_warmup,
            "HTTP memory grew after warmup"
        );
        for _ in 0..WARMUP_CALLS {
            ensure!(
                counted_run
                    .call_async(&mut counted_store, ("measurement.invalid", limit as f64))
                    .await?
                    .0
                    == expected,
                "instrumented HTTP outcome differs"
            );
        }
        let before = counters.snapshot(&mut counted_store, &counted).await?;
        let (result, polls) = call_counts::count_polls(
            counted_run.call_async(&mut counted_store, ("measurement.invalid", limit as f64)),
        )
        .await;
        ensure!(result?.0 == expected, "instrumented HTTP outcome differs");
        counted_store.assert_concurrent_state_empty();
        ensure!(
            counted_store.data().table.is_empty(),
            "instrumented HTTP resources remain"
        );
        let after = counters.snapshot(&mut counted_store, &counted).await?;
        measurements.push(Measurement {
            workload,
            component_bytes,
            memory_limit_bytes: store.data().memory.limit,
            memory_after_warmup,
            memory_after_samples: store.data().memory.allocated,
            warmup_calls: WARMUP_CALLS,
            calls_per_sample: CALLS_PER_SAMPLE,
            payload_bytes: PAYLOAD_BYTES,
            milliseconds_per_call,
            counts: Some(counters.difference(&before, &after, polls)),
        });
    }
    ensure!(
        store.data().response.requests
            == outcomes.len() * (WARMUP_CALLS + SAMPLE_COUNT * CALLS_PER_SAMPLE),
        "missing HTTP requests"
    );
    Ok(measurements)
}

fn http_store(engine: &Engine) -> Store<Host> {
    let mut store = Store::new(
        engine,
        Host {
            wasi: WasiCtx::default(),
            table: ResourceTable::new(),
            http: WasiHttpCtx::new(),
            response: ResponseFixture {
                body: Bytes::from((0..PAYLOAD_BYTES).map(|i| i as u8).collect::<Vec<_>>()),
                requests: 0,
            },
            memory: MemoryUsage {
                allocated: 0,
                limit: 16 * 1024 * 1024,
            },
        },
    );
    store.limiter(|host| &mut host.memory);
    store
}
