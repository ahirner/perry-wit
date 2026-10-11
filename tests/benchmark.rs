//! The benchmark baseline (`TODOs.md` P0.2), one command:
//!
//! ```text
//! cargo test --test benchmark -- --ignored --nocapture
//! ```
//!
//! It prints a Markdown table: component bytes (stripped), guest fuel per call and guest
//! memory, which are deterministic and machine-independent, and median milliseconds per call
//! for orientation. The committed table lives in `TODOs.md`. Workloads: the legacy
//! measurement set (text, file I/O, a 4 MiB `stream<u8>`, a 4 MiB HTTP body read bounded and
//! whole), the critique microbenchmarks as core modules, the compile time of a deep property
//! chain, and the example components.
use anyhow::{Result, ensure};
use perry_wit::{compile_core, compile_for_world, component::wit::resolve_wit};
use std::{future::Future, time::Instant};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, Val,
    component::{Component, Linker, ResourceTable, StreamReader},
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

#[allow(dead_code)]
#[path = "support/http_fixture.rs"]
mod http_fixture;
use http_fixture::{HttpFixture, Reply};

const MIB4: usize = 4 * 1024 * 1024;
const SAMPLES: usize = 5;

/// Linear memory the guest has grown to, in bytes.
#[derive(Default)]
struct Memory {
    bytes: usize,
    initial: Option<usize>,
}

impl ResourceLimiter for Memory {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.initial.get_or_insert(desired);
        self.bytes = self.bytes.max(desired);
        Ok(true)
    }
    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

struct Host {
    ctx: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
    memory: Memory,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

#[derive(Default, serde::Serialize)]
struct Row {
    workload: String,
    bytes: Option<usize>,
    fuel: Option<u64>,
    memory: Option<usize>,
    milliseconds: Option<f64>,
    initial_memory: Option<usize>,
    compile_milliseconds: Option<f64>,
    counters: Option<Vec<u64>>,
}

impl Row {
    fn markdown(&self) -> String {
        let number = |value: Option<u64>| {
            value.map_or("–".into(), |v| {
                let digits = v.to_string();
                let mut out = String::new();
                for (i, c) in digits.chars().enumerate() {
                    if i > 0 && (digits.len() - i) % 3 == 0 {
                        out.push(',');
                    }
                    out.push(c);
                }
                out
            })
        };
        format!(
            "| {} | {} | {} | {} | {} |",
            self.workload,
            number(self.bytes.map(|b| b as u64)),
            number(self.fuel),
            number(self.memory.map(|b| b as u64)),
            self.milliseconds
                .map_or("–".into(), |ms| format!("{ms:.3}"))
        )
    }
}

fn engine(fuel: bool) -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model_async(true).consume_fuel(fuel);
    Ok(Engine::new(&config)?)
}

fn store(engine: &Engine, root: Option<&std::path::Path>, metered: bool) -> Result<Store<Host>> {
    let mut ctx = WasiCtx::builder();
    if let Some(root) = root {
        ctx.preopened_dir(root, "/", FsPerms::ReadWrite)?;
    }
    let mut store = Store::new(
        engine,
        Host {
            ctx: ctx.build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            memory: Memory::default(),
        },
    );
    store.limiter(|host| &mut host.memory);
    if metered {
        store.set_fuel(u64::MAX)?;
    }
    Ok(store)
}

fn component(source: &str, wit: &str, world: &str) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), wit)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some(world))?;
    Ok(
        compile_for_world(source, "bench.ts", &resolve, world, true)?
            .component
            .unwrap()
            .stripped,
    )
}

/// Measure `call` on a fresh instance per engine: fuel and memory of one call after a
/// warm-up call, and the median of `SAMPLES` timed calls.
async fn measure<F, Fut>(
    workload: &str,
    bytes: &[u8],
    root: Option<&std::path::Path>,
    call: F,
) -> Result<Row>
where
    F: Fn(Store<Host>, Component, Linker<Host>) -> Fut,
    Fut: Future<Output = Result<(Store<Host>, Vec<std::time::Duration>, u64)>>,
{
    let mut fuel = None;
    let mut memory = None;
    let mut milliseconds = None;
    let mut initial_memory = None;
    for metered in [true, false] {
        let engine = engine(metered)?;
        let component = Component::new(&engine, bytes)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        let (store, times, used) = call(store(&engine, root, metered)?, component, linker).await?;
        if metered {
            fuel = Some(used);
            memory = Some(store.data().memory.bytes);
            initial_memory = store.data().memory.initial;
        } else {
            let mut ms: Vec<f64> = times.iter().map(|t| t.as_secs_f64() * 1000.0).collect();
            ms.sort_by(f64::total_cmp);
            milliseconds = ms.get(ms.len() / 2).copied();
        }
    }
    Ok(Row {
        workload: workload.into(),
        bytes: Some(bytes.len()),
        fuel,
        memory,
        milliseconds,
        initial_memory,
        ..Row::default()
    })
}

/// Fuel spent by `store` since `before`, or 0 without metering.
fn spent(store: &Store<Host>, before: u64) -> u64 {
    store.get_fuel().map_or(0, |after| before - after)
}

macro_rules! timed {
    ($store:ident, $calls:expr, $body:expr) => {{
        // Warm-up, then one metered call, then the timed samples.
        $body;
        let before = $store.get_fuel().unwrap_or(0);
        $body;
        let used = spent(&$store, before);
        let mut times = Vec::new();
        for _ in 0..$calls {
            let started = Instant::now();
            $body;
            times.push(started.elapsed());
        }
        (times, used)
    }};
}

async fn components() -> Result<Vec<Row>> {
    let mut rows = Vec::new();

    // text: a synchronous export appending to its argument.
    let text = component(
        r#"export function runTask(input: string): string {
  let result = input;
  for (let index = 0; index < 64; index++) result = result + "!";
  return result;
}"#,
        "package test:bench; world task { include wasi:cli/imports@0.3.0; export run-task: func(input: string) -> string; }",
        "task",
    )?;
    let input = "abcdefghijklmnopqrstuvwxyz0123456789".repeat(128)[..4096].to_owned();
    let expected = format!("{input}{}", "!".repeat(64));
    rows.push(
        measure("text", &text, None, |mut store, component, linker| {
            let (input, expected) = (input.clone(), expected.clone());
            async move {
                let instance = linker.instantiate_async(&mut store, &component).await?;
                let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
                let (times, used) = timed!(store, SAMPLES, {
                    let (result,) = run.call_async(&mut store, (input.as_str(),)).await?;
                    ensure!(result == expected, "text result");
                });
                Ok((store, times, used))
            }
        })
        .await?,
    );

    // io: read a 64 KiB file and write a copy.
    let io = component(
        r#"import { readFile, writeFile } from 'node:fs/promises';
export async function runTask(path: string): Promise<string> {
  const data = await readFile(path, 'utf8');
  await writeFile(path + '.copy', data);
  return data;
}"#,
        "package test:bench; world task { include wasi:cli/imports@0.3.0; export run-task: async func(input: string) -> string; }",
        "task",
    )?;
    let directory = tempfile::tempdir()?;
    let data = "abcdefgh01234567".repeat(4096);
    std::fs::write(directory.path().join("input"), &data)?;
    rows.push(
        measure(
            "io (64 KiB)",
            &io,
            Some(directory.path()),
            |mut store, component, linker| {
                let data = data.clone();
                async move {
                    let instance = linker.instantiate_async(&mut store, &component).await?;
                    let run =
                        instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
                    let (times, used) = timed!(store, SAMPLES, {
                        let (result,) = run.call_async(&mut store, ("/input",)).await?;
                        ensure!(result == data, "io result");
                    });
                    Ok((store, times, used))
                }
            },
        )
        .await?,
    );

    // incoming-stream: sum 4 MiB of `stream<u8>`, one chunk alive at a time.
    let stream = component(
        r#"export async function run(input: ReadableStream<Uint8Array>): Promise<number> {
  const reader = input.getReader();
  let sum = 0;
  let result = await reader.read();
  while (!result.done) {
    const bytes = result.value;
    if (bytes === undefined) throw 1;
    for (let i = 0; i < bytes.length; i++) { const b = bytes[i]; if (b !== undefined) sum += b; }
    result = await reader.read();
  }
  reader.releaseLock();
  return sum;
}"#,
        "package test:bench; world task { include wasi:cli/imports@0.3.0; export run: async func(input: stream<u8>) -> f64; }",
        "task",
    )?;
    let payload: Vec<u8> = (0..MIB4).map(|i| (i * 37 + i / 251) as u8).collect();
    let sum: f64 = payload.iter().map(|&b| f64::from(b)).sum();
    rows.push(
        measure(
            "incoming-stream (4 MiB)",
            &stream,
            None,
            |mut store, component, linker| {
                let payload = payload.clone();
                async move {
                    let instance = linker.instantiate_async(&mut store, &component).await?;
                    let run = instance
                        .get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
                    let (times, used) = timed!(store, SAMPLES, {
                        let input = StreamReader::new(&mut store, payload.clone())?;
                        let (result,) = run.call_async(&mut store, (input,)).await?;
                        ensure!(result == sum, "stream sum");
                    });
                    Ok((store, times, used))
                }
            },
        )
        .await?,
    );

    // HTTP: a 4 MiB body read with a bound (exact and one byte over) and whole.
    let body: Vec<u8> = (0..MIB4).map(|i| i as u8).collect();
    let served = body.clone();
    let server = HttpFixture::new(move |_| Reply::Bytes(200, served.clone()));
    let url = format!("http://{}/", server.address);
    let http_wit = "package test:bench; world task { include wasi:cli/imports@0.3.0; import wasi:http/client@0.3.0; export run: async func(url: string, limit: f64) -> f64; }";
    let bounded = component(
        &format!(
            r#"{}
export async function run(url: string, limit: number): Promise<number> {{
  try {{
    const response = await fetch(url);
    const body = await readBounded(response, limit);
    if (response.status !== 200) throw 91;
    return body.length;
  }} catch (error) {{ return -8; }}
}}"#,
            include_str!("fixtures/bounded_response.ts")
        ),
        http_wit,
        "task",
    )?;
    let whole = component(
        r#"export async function run(url: string, limit: number): Promise<number> {
  const response = await fetch(url);
  const body = await response.bytes();
  if (response.status !== 200) throw 91;
  return body.length + limit * 0;
}"#,
        http_wit,
        "task",
    )?;
    for (workload, bytes, limit, expected) in [
        ("bounded-http-exact (4 MiB)", &bounded, MIB4, MIB4 as f64),
        ("bounded-http-overflow", &bounded, MIB4 - 1, -8.0),
        ("unbounded-http-bytes (4 MiB)", &whole, 0, MIB4 as f64),
    ] {
        rows.push(
            measure(workload, bytes, None, |mut store, component, linker| {
                let url = url.clone();
                async move {
                    let instance = linker.instantiate_async(&mut store, &component).await?;
                    let run = instance.get_typed_func::<(&str, f64), (f64,)>(&mut store, "run")?;
                    let (times, used) = timed!(store, SAMPLES, {
                        let (result,) = run
                            .call_async(&mut store, (url.as_str(), limit as f64))
                            .await?;
                        ensure!(result == expected, "{workload}: {result}");
                    });
                    Ok((store, times, used))
                }
            })
            .await?,
        );
    }
    Ok(rows)
}

/// The critique's microbenchmarks as raw core modules, at n = 10⁶.
fn microbenchmarks() -> Result<Vec<Row>> {
    let workloads = [
        (
            "scalar % loop",
            r#"export function run(n: number): number {
  let acc = 0;
  for (let i = 0; i < n; i++) acc = (acc + i * 3) % 1000003;
  return acc;
}"#,
        ),
        (
            "temporary record per iteration",
            r#"interface Point { x: number; y: number }
function make(i: number): Point { return { x: i, y: i * 2 }; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) { const p = make(i); sum = (sum + p.x + p.y) % 1000003; }
  return sum;
}"#,
        ),
        (
            "push records, then sum",
            r#"interface Point { x: number; y: number }
export function run(n: number): number {
  const items: Point[] = [];
  for (let i = 0; i < n; i++) items.push({ x: i, y: i * 2 });
  let sum = 0;
  for (let k = 0; k < items.length; k++) { const p = items[k]; sum = (sum + p.x + p.y) % 1000003; }
  return sum;
}"#,
        ),
        (
            "push numbers, then sum",
            r#"export function run(n: number): number {
  const items: number[] = [];
  for (let i = 0; i < n; i++) items.push(i * 3);
  let sum = 0;
  for (let k = 0; k < items.length; k++) sum = (sum + items[k]) % 1000003;
  return sum;
}"#,
        ),
        (
            "allocation-free direct calls",
            r#"export function step(i: number): number { if (i < 0) return 0; return i * 3; }
export function retain(n: number): number { const a = [n]; return a.length; }
export function run(n: number): number {
  let sum = 0;
  for (let i = 0; i < n; i++) sum += step(i);
  return sum;
}"#,
        ),
        (
            "byte indexing",
            r#"export function run(n: number): number {
  const bytes = new Uint8Array(n);
  for (let i = 0; i < n; i++) bytes[i] = i;
  let sum = 0;
  for (let i = 0; i < bytes.length; i++) sum += bytes[i];
  return sum;
}"#,
        ),
        (
            "string append",
            r#"export function run(n: number): number {
  let text = "";
  for (let i = 0; i < n; i++) text = text + "x";
  return text.length;
}"#,
        ),
    ];
    let mut rows = Vec::new();
    for (workload, source) in workloads {
        let started = Instant::now();
        let wasm = compile_core(source, "bench.ts")?;
        let compile_ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut row = Row {
            workload: format!("{workload} (n = 10⁶)"),
            bytes: Some(wasm.len()),
            fuel: None,
            memory: None,
            milliseconds: None,
            ..Row::default()
        };
        row.compile_milliseconds = Some(compile_ms);
        for metered in [true, false] {
            let mut config = Config::new();
            config.consume_fuel(metered);
            let engine = Engine::new(&config)?;
            let module = wasmtime::Module::new(&engine, &wasm)?;
            let mut store = Store::new(&engine, Memory::default());
            store.limiter(|memory| memory);
            if metered {
                store.set_fuel(u64::MAX)?;
            }
            let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
            if metered {
                row.initial_memory = Some(store.data().bytes);
            }
            let run = instance.get_func(&mut store, "run").unwrap();
            let mut results = vec![Val::I32(0); run.ty(&store).results().len()];
            let n = Val::F64(1_000_000f64.to_bits());
            let started = Instant::now();
            run.call(&mut store, &[n], &mut results)?;
            let elapsed = started.elapsed();
            let expected = if workload == "string append" {
                1_000_000.0
            } else if workload == "allocation-free direct calls" {
                3.0 * 999_999.0 * 1_000_000.0 / 2.0
            } else if workload == "byte indexing" {
                (0..1_000_000u64).map(|i| i & 255).sum::<u64>() as f64
            } else {
                (0..1_000_000u64).fold(0, |sum, i| (sum + 3 * i) % 1000003) as f64
            };
            ensure!(
                results[0].unwrap_f64() == expected,
                "{workload}: wrong result {results:?}"
            );
            if results.len() > 1 {
                ensure!(results[1].unwrap_i32() == 0, "{workload}: exception");
            }
            if metered {
                row.fuel = Some(u64::MAX - store.get_fuel()?);
                let memory = instance.get_memory(&mut store, "memory");
                row.memory = Some(memory.map_or(store.data().bytes, |m| {
                    m.data_size(&store).max(store.data().bytes)
                }));
            } else {
                row.milliseconds = Some(elapsed.as_secs_f64() * 1000.0);
            }
        }
        // Counters use a separate instrumented module, never the ordinary fuel result.
        let checked = perry_wit::compiler::compile_core_with_memory_checks(source, "bench.ts")?;
        let engine = engine(false)?;
        let module = wasmtime::Module::new(&engine, &checked)?;
        let mut store = Store::new(&engine, ());
        let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
        instance
            .get_typed_func::<i32, ()>(&mut store, "__perry_gc_debug")?
            .call(&mut store, 4)?;
        let run = instance.get_func(&mut store, "run").unwrap();
        let mut results = vec![Val::I32(0); run.ty(&store).results().len()];
        run.call(
            &mut store,
            &[Val::F64(1_000_000f64.to_bits())],
            &mut results,
        )?;
        let counter = instance.get_typed_func::<i32, i64>(&mut store, "__perry_gc_counter")?;
        row.counters = Some(
            (0..11)
                .map(|i| counter.call(&mut store, i).map(|v| v as u64))
                .collect::<Result<Vec<_>, _>>()?,
        );
        rows.push(row);
    }
    Ok(rows)
}

/// Compile time of a property chain 32 levels deep (exponential on the legacy backend).
fn compile_time() -> Result<Row> {
    let mut source = String::from("interface N0 { v: number }\n");
    for level in 1..=32 {
        source.push_str(&format!("interface N{level} {{ c: N{} }}\n", level - 1));
    }
    source.push_str("export function run(o: N32): number { return o");
    for _ in 0..32 {
        source.push_str(".c");
    }
    source.push_str(".v; }\n");
    let started = Instant::now();
    compile_core(&source, "chain.ts")?;
    Ok(Row {
        workload: "compile: property chain, depth 32".into(),
        bytes: None,
        fuel: None,
        memory: None,
        milliseconds: Some(started.elapsed().as_secs_f64() * 1000.0),
        ..Row::default()
    })
}

/// Stripped sizes of the example components.
fn examples() -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for (workload, source, wit, world) in [
        (
            "example: merge_task",
            "examples/merge_task/index.ts",
            "wit",
            "task-runner",
        ),
        (
            "example: template task",
            "template/src/index.ts",
            "template/wit",
            "task",
        ),
    ] {
        let (resolve, package) = resolve_wit(std::path::Path::new(wit))?;
        let world = resolve.select_world(&[package], Some(world))?;
        let source_text = std::fs::read_to_string(source)?;
        let bytes = compile_for_world(&source_text, source, &resolve, world, true)?
            .component
            .unwrap()
            .stripped;
        rows.push(Row {
            workload: workload.into(),
            bytes: Some(bytes.len()),
            fuel: None,
            memory: None,
            milliseconds: None,
            ..Row::default()
        });
    }
    Ok(rows)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "performance campaign; run explicitly"]
async fn benchmark_baseline() -> Result<()> {
    let mut rows = components().await?;
    rows.extend(microbenchmarks()?);
    rows.push(compile_time()?);
    rows.extend(examples()?);
    println!("| Workload | Component bytes | Fuel per call | Guest memory bytes | Median ms |");
    println!("|---|---:|---:|---:|---:|");
    if let Ok(path) = std::env::var("PERRY_BENCH_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&rows)?)?;
    }
    for row in rows {
        println!("{}", row.markdown());
    }
    Ok(())
}
