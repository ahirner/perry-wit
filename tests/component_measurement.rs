//! Opt-in size, memory, and throughput measurements of production components.

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use perry_wit::{CompileOptions, compile_file};
use serde::Serialize;
use std::{fs, future::Future, path::Path, time::Instant};
use wasmtime_wasi_http::{WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView};
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
    timings: Vec<Measurement>,
    sizes: Vec<SizeMeasurement>,
}
#[derive(Serialize)]
struct Measurement {
    pipeline: &'static str,
    workload: &'static str,
    component_bytes: usize,
    memory_limit_bytes: usize,
    memory_after_warmup: usize,
    memory_after_samples: usize,
    warmup_calls: usize,
    calls_per_sample: usize,
    payload_bytes: usize,
    milliseconds_per_call: Vec<f64>,
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "set PERRY_MEASUREMENT_OUTPUT to record size, memory, and throughput"]
async fn measure_components() -> Result<()> {
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
            memory_limit_bytes: store.data().memory.limit,
            memory_after_warmup,
            memory_after_samples: store.data().memory.allocated,
            warmup_calls: 5,
            calls_per_sample,
            payload_bytes,
            milliseconds_per_call,
        };
        eprintln!("{}", serde_json::to_string(&result)?);
        measurements.push(result);
    }
    let legacy = fs::read_to_string("types/p3.d.ts")?.contains("declare module \"perry:http\"");
    measurements.push(measure_stream(&engine, artifacts, legacy, false).await?);
    measurements.extend(measure_http(&engine, artifacts, legacy, false).await?);
    if !legacy {
        measurements.push(measure_stream(&engine, artifacts, false, true).await?);
        measurements.extend(measure_http(&engine, artifacts, false, true).await?);
    }

    let mut sizes = Vec::new();
    for (workload, source, wit, world) in [
        ("merge_docs.ts", "examples/merge_docs.ts", "wit", "command"),
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

// Historical source is only compiled by the archived compiler that declares these APIs.
const LEGACY_STREAM: &str = r#"
declare function readChunk(input: ByteStream): Promise<number>;
declare function byteAt(index: number): number;
export async function run(input: ByteStream): Promise<number> {
    let total=0;
    let length=await readChunk(input);
    while(length>0) {
        for(let index=0;index<length;index++)total+=byteAt(index);
        length=await readChunk(input);
    }
    return total;
}
"#;
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

async fn measure_stream(
    engine: &Engine,
    artifacts: &Path,
    legacy: bool,
    byob: bool,
) -> Result<Measurement> {
    use wasmtime::component::StreamReader;
    let source = if legacy {
        LEGACY_STREAM
    } else if byob {
        BYOB_STREAM
    } else {
        STREAM
    };
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
        perry_wit::strip::component(&compiled.component.context("stream component")?)?;
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
    Ok(Measurement {
        pipeline: "production",
        workload,
        component_bytes: stream_size,
        memory_limit_bytes: store.data().limit,
        memory_after_warmup,
        memory_after_samples: store.data().allocated,
        warmup_calls: WARMUP_CALLS,
        calls_per_sample: CALLS_PER_SAMPLE,
        payload_bytes: PAYLOAD_BYTES,
        milliseconds_per_call,
    })
}

async fn measure_http(
    engine: &Engine,
    artifacts: &Path,
    legacy: bool,
    byob: bool,
) -> Result<Vec<Measurement>> {
    let read = if legacy {
        "const response=await get('http',authority,'/',{},limit);const body=response.body;"
    } else {
        "const response=await fetch('http://'+authority+'/');const body=await readBounded(response,limit);"
    };
    let prefix = if legacy {
        "import {get} from 'perry:http';"
    } else if byob {
        include_str!("fixtures/bounded_byob_response.ts")
    } else {
        include_str!("fixtures/bounded_response.ts")
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
    let name = if byob {
        "bounded-http-byob"
    } else {
        "bounded-http"
    };
    fs::write(artifacts.join(format!("{name}.ts")), &source)?;
    let wit = tempfile::tempdir()?;
    fs::write(
        wit.path().join("world.wit"),
        "package test:measurement; world task {import wasi:http/client@0.3.0; export run:async func(authority:string,limit:f64)->f64;}",
    )?;
    let bytes = perry_wit::compile_typescript(
        &source,
        "bounded-http.ts",
        &CompileOptions {
            wit_dir: wit.path().into(),
            world: Some("task".into()),
            core_only: false,
        },
    )?
    .stripped
    .context("stripped HTTP component")?;
    fs::write(artifacts.join(format!("{name}.wasm")), &bytes)?;
    let component_bytes = bytes.len();
    let component = Component::new(engine, bytes)?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
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
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str, f64), (f64,)>(&mut store, "run")?;
    let mut measurements = Vec::new();
    for (workload, limit, expected) in [
        (
            if byob {
                "bounded-http-exact-byob"
            } else {
                "bounded-http-exact"
            },
            PAYLOAD_BYTES,
            PAYLOAD_BYTES as f64,
        ),
        (
            if byob {
                "bounded-http-overflow-byob"
            } else {
                "bounded-http-overflow"
            },
            PAYLOAD_BYTES - 1,
            -8.0,
        ),
    ] {
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
        measurements.push(Measurement {
            pipeline: "production",
            workload,
            component_bytes,
            memory_limit_bytes: store.data().memory.limit,
            memory_after_warmup,
            memory_after_samples: store.data().memory.allocated,
            warmup_calls: WARMUP_CALLS,
            calls_per_sample: CALLS_PER_SAMPLE,
            payload_bytes: PAYLOAD_BYTES,
            milliseconds_per_call,
        });
    }
    ensure!(
        store.data().response.requests == 2 * (WARMUP_CALLS + SAMPLE_COUNT * CALLS_PER_SAMPLE),
        "missing HTTP requests"
    );
    Ok(measurements)
}
