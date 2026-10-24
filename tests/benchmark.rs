//! Deterministic compiler and guest benchmarks.
//! Run with `cargo test --test benchmark -- --ignored --nocapture`.
//!
//! Every workload's sizes, fuel, memory and optimization statistics must equal
//! `tests/benchmark.json`. `PERRY_EXPECT_BENCH=1` rewrites that record instead, so its git
//! history tracks each change. `bytes` measures stripped components or named raw modules;
//! `core_bytes` the named core module. `initialization_fuel` measures instantiation,
//! `invocation_fuel` the first export call and `warm_fuel` the second component call.
//!
//! The printed table adds median milliseconds per call for orientation; timing is never
//! recorded. Workloads: component exports (text, file I/O, a 4 MiB `stream<u8>`, a 4 MiB
//! HTTP body read bounded and whole), microbenchmarks as core modules, the compile time of a
//! deep property chain, the example components and the optimization fixtures.
use anyhow::{Context as _, Result, bail, ensure};
use http_body_util::{BodyExt, Full};
use perry_wit::{compile_core, compile_for_world, component::wit::resolve_wit};
use std::{future::Future, time::Instant};
use wasmtime::{
    Config, Engine, ResourceLimiter, Store, Val,
    component::{Component, Instance, Linker, ResourceTable, StreamReader},
};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{
    Error, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

const RECORD: &str = "tests/benchmark.json";
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
    fixture: FixedHttp,
}

struct FixedHttp;

impl WasiHttpHooks for FixedHttp {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _: Option<RequestOptions>,
        _: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), Error>> + Send>,
                    ),
                    Error,
                >,
            > + Send,
    > {
        assert_eq!(request.uri(), "http://benchmark.test/body");
        Box::new(async {
            let bytes: Vec<u8> = (0..MIB4).map(|i| i as u8).collect();
            let body = Full::new(bytes::Bytes::from(bytes))
                .map_err(|never| match never {})
                .boxed_unsync();
            Ok((
                http::Response::builder()
                    .header("content-length", MIB4)
                    .body(body)
                    .unwrap(),
                Box::new(async { Ok(()) }) as Box<dyn Future<Output = _> + Send>,
            ))
        })
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

impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.fixture,
        }
    }
}

#[derive(Default, serde::Serialize)]
struct Row {
    workload: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    core_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    defined_functions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    helper_functions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    optimization: Option<std::collections::BTreeMap<String, u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initialization_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    invocation_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warm_fuel: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_memory: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    counters: Option<Vec<u64>>,
    #[serde(skip)]
    milliseconds: Option<f64>,
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
            number(self.invocation_fuel),
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
            fixture: FixedHttp,
        },
    );
    store.limiter(|host| &mut host.memory);
    if metered {
        store.set_fuel(u64::MAX)?;
    }
    Ok(store)
}

fn component(source: &str, wit: &str, world: &str) -> Result<perry_wit::Compiled> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), wit)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some(world))?;
    Ok(compile_for_world(
        source, "bench.ts", &resolve, world, true,
    )?)
}

fn optimization_statistics(report: &str) -> Result<std::collections::BTreeMap<String, u64>> {
    let line = report
        .lines()
        .find_map(|line| line.strip_prefix("Optimization statistics: "))
        .ok_or_else(|| anyhow::anyhow!("missing optimization statistics"))?;
    line.split(", ")
        .map(|entry| {
            let (key, value) = entry
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("invalid statistic {entry}"))?;
            Ok((key.to_string(), value.parse()?))
        })
        .collect()
}

fn helper_functions(report: &str) -> Result<u32> {
    Ok(report
        .lines()
        .find_map(|line| line.strip_prefix("Linked helper functions: "))
        .ok_or_else(|| anyhow::anyhow!("missing helper count"))?
        .parse()?)
}

fn defined_functions(core: &[u8]) -> Result<u32> {
    for payload in wasmparser::Parser::new(0).parse_all(core) {
        if let wasmparser::Payload::FunctionSection(functions) = payload? {
            return Ok(functions.count());
        }
    }
    Ok(0)
}

/// Measure instantiation, first invocation, the warmed call, and timed samples.
async fn measure<F, Fut>(
    workload: &str,
    compiled: &perry_wit::Compiled,
    root: Option<&std::path::Path>,
    call: F,
) -> Result<Row>
where
    F: Fn(Store<Host>, Instance) -> Fut,
    Fut: Future<Output = Result<(Store<Host>, Vec<std::time::Duration>, u64, u64)>>,
{
    let bytes = &compiled
        .component
        .as_ref()
        .expect("component benchmark")
        .stripped;
    let mut warm_fuel = None;
    let mut initialization_fuel = None;
    let mut invocation_fuel = None;
    let mut memory = None;
    let mut milliseconds = None;
    let mut initial_memory = None;
    for metered in [true, false] {
        let engine = engine(metered)?;
        let component = Component::new(&engine, bytes)?;
        let mut linker = Linker::new(&engine);
        wasmtime_wasi::p3::add_to_linker(&mut linker)?;
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
        let mut store = store(&engine, root, metered)?;
        let before = store.get_fuel().unwrap_or(0);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let initialization = spent(&store, before);
        let (store, times, used, first_used) = call(store, instance).await?;
        if metered {
            warm_fuel = Some(used);
            initialization_fuel = Some(initialization);
            invocation_fuel = Some(first_used);
            memory = Some(store.data().memory.bytes);
            initial_memory = Some(store.data().memory.initial.unwrap_or(0));
        } else {
            let mut ms: Vec<f64> = times.iter().map(|t| t.as_secs_f64() * 1000.0).collect();
            ms.sort_by(f64::total_cmp);
            milliseconds = ms.get(ms.len() / 2).copied();
        }
    }
    Ok(Row {
        workload: workload.into(),
        bytes: Some(bytes.len()),
        core_bytes: Some(compiled.core.len()),
        defined_functions: Some(defined_functions(&compiled.core)?),
        helper_functions: Some(helper_functions(&compiled.allocation_report)?),
        optimization: Some(optimization_statistics(&compiled.allocation_report)?),
        initialization_fuel,
        invocation_fuel,
        warm_fuel,
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
        // The first call is also the timing warm-up.
        let before = $store.get_fuel().unwrap_or(0);
        $body;
        let first_used = spent(&$store, before);
        let before = $store.get_fuel().unwrap_or(0);
        $body;
        let used = spent(&$store, before);
        let mut times = Vec::new();
        for _ in 0..$calls {
            let started = Instant::now();
            $body;
            times.push(started.elapsed());
        }
        (times, used, first_used)
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
        measure("text", &text, None, |mut store, instance| {
            let (input, expected) = (input.clone(), expected.clone());
            async move {
                let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
                let (times, used, first_used) = timed!(store, SAMPLES, {
                    let (result,) = run.call_async(&mut store, (input.as_str(),)).await?;
                    ensure!(result == expected, "text result");
                });
                Ok((store, times, used, first_used))
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
            |mut store, instance| {
                let data = data.clone();
                async move {
                    let run =
                        instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
                    let (times, used, first_used) = timed!(store, SAMPLES, {
                        let (result,) = run.call_async(&mut store, ("/input",)).await?;
                        ensure!(result == data, "io result");
                    });
                    Ok((store, times, used, first_used))
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
            |mut store, instance| {
                let payload = payload.clone();
                async move {
                    let run = instance
                        .get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
                    let (times, used, first_used) = timed!(store, SAMPLES, {
                        let input = StreamReader::new(&mut store, payload.clone())?;
                        let (result,) = run.call_async(&mut store, (input,)).await?;
                        ensure!(result == sum, "stream sum");
                    });
                    Ok((store, times, used, first_used))
                }
            },
        )
        .await?,
    );

    // HTTP: a 4 MiB body read with a bound (exact and one byte over) and whole.
    let url = "http://benchmark.test/body".to_owned();
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
            measure(workload, bytes, None, |mut store, instance| {
                let url = url.clone();
                async move {
                    let run = instance.get_typed_func::<(&str, f64), (f64,)>(&mut store, "run")?;
                    let (times, used, first_used) = timed!(store, SAMPLES, {
                        let (result,) = run
                            .call_async(&mut store, (url.as_str(), limit as f64))
                            .await?;
                        ensure!(result == expected, "{workload}: {result}");
                    });
                    Ok((store, times, used, first_used))
                }
            })
            .await?,
        );
    }
    Ok(rows)
}

/// Microbenchmarks as raw core modules, at n = 10⁶.
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
        let (wasm, mut row) = raw_module(format!("{workload} (n = 10⁶)"), source)?;
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
                row.initialization_fuel = Some(u64::MAX - store.get_fuel()?);
            }
            let run = instance.get_func(&mut store, "run").unwrap();
            let mut results = vec![Val::I32(0); run.ty(&store).results().len()];
            let n = Val::F64(1_000_000f64.to_bits());
            let before = store.get_fuel().unwrap_or(0);
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
                row.invocation_fuel = Some(before - store.get_fuel()?);
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

/// A raw core module and its static metrics: sizes, function counts and optimization
/// statistics. The linked-minus-emitted function count measures helpers without naming
/// heuristics.
fn raw_module(workload: String, source: &str) -> Result<(Vec<u8>, Row)> {
    use pliron::{context::Context, op::Op, printable::Printable};
    let wasm = compile_core(source, "bench.ts")?;
    // Inspect the same raw pipeline separately.
    let ctx = &mut Context::new();
    let mir = perry_lower::lower_sources(
        ctx,
        source,
        "bench.ts",
        &Default::default(),
        &Default::default(),
    )?;
    let mut analyses = perry_mir::canonicalize(mir.get_operation(), ctx)
        .map_err(|error| anyhow::anyhow!("{}", error.disp(ctx)))?;
    let exports = perry_codegen::Exports::Raw;
    let optimization = optimization_statistics(&perry_codegen::explain_tiers(
        ctx,
        &mir,
        &exports,
        &mut analyses,
    )?)?;
    let emitted = perry_codegen::compile(ctx, &mir, &exports, &mut analyses)?;
    let row = Row {
        workload,
        bytes: Some(wasm.len()),
        core_bytes: Some(wasm.len()),
        defined_functions: Some(defined_functions(&wasm)?),
        helper_functions: Some(defined_functions(&wasm)? - defined_functions(&emitted)?),
        optimization: Some(optimization),
        ..Row::default()
    };
    Ok((wasm, row))
}

/// Compile time of a property chain 32 levels deep, with the module's static metrics.
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
    let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
    let (_, row) = raw_module("compile: property chain, depth 32".into(), &source)?;
    Ok(Row {
        milliseconds: Some(milliseconds),
        ..row
    })
}

/// Stripped sizes of the example components.
fn examples() -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for (workload, source, wit, world) in [
        (
            "example: merge_task",
            "examples/merge_task/index.ts",
            "examples/merge_task",
            "merge-task",
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
        let compiled = compile_for_world(&source_text, source, &resolve, world, true)?;
        rows.push(Row {
            workload: workload.into(),
            bytes: Some(compiled.component.unwrap().stripped.len()),
            core_bytes: Some(compiled.core.len()),
            defined_functions: Some(defined_functions(&compiled.core)?),
            helper_functions: Some(helper_functions(&compiled.allocation_report)?),
            optimization: Some(optimization_statistics(&compiled.allocation_report)?),
            ..Row::default()
        });
    }
    Ok(rows)
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "performance campaign; run explicitly"]
async fn benchmark() -> Result<()> {
    let mut rows = components().await?;
    rows.extend(microbenchmarks()?);
    rows.push(compile_time()?);
    rows.extend(examples()?);
    rows.extend(optimization_fixtures().await?);
    println!("| Workload | Component bytes | Fuel per call | Guest memory bytes | Median ms |");
    println!("|---|---:|---:|---:|---:|");
    for row in &rows {
        println!("{}", row.markdown());
    }
    let current = serde_json::to_string_pretty(&rows)? + "\n";
    match std::env::var("PERRY_EXPECT_BENCH") {
        Ok(value) if value == "1" => Ok(std::fs::write(RECORD, current)?),
        Err(std::env::VarError::NotPresent) => {
            let recorded = std::fs::read_to_string(RECORD)?;
            ensure!(
                recorded == current,
                "{RECORD} differs; rerun with PERRY_EXPECT_BENCH=1 and commit the record if the change is intended:\n{}",
                differences(&recorded, &current)?.join("\n")
            );
            Ok(())
        }
        other => bail!("PERRY_EXPECT_BENCH must be 1, found {other:?}"),
    }
}

/// One line per workload field whose recorded and current values differ.
fn differences(recorded: &str, current: &str) -> Result<Vec<String>> {
    use serde_json::{Map, Value};
    use std::collections::{BTreeMap, BTreeSet};
    let index = |text: &str| -> Result<BTreeMap<String, Map<String, Value>>> {
        let rows: Vec<Map<String, Value>> = serde_json::from_str(text)?;
        rows.into_iter()
            .map(|row| {
                let workload = row.get("workload").and_then(Value::as_str);
                Ok((workload.context("row without workload")?.to_owned(), row))
            })
            .collect()
    };
    let (before, after) = (index(recorded)?, index(current)?);
    let mut lines = Vec::new();
    for workload in before.keys().chain(after.keys()).collect::<BTreeSet<_>>() {
        let (old, new) = (before.get(workload), after.get(workload));
        let fields: BTreeSet<_> = old.into_iter().chain(new).flat_map(Map::keys).collect();
        for field in fields {
            let value = |row: Option<&Map<String, Value>>| {
                row.and_then(|row| row.get(field))
                    .map_or("–".into(), Value::to_string)
            };
            let (old, new) = (value(old), value(new));
            if old != new {
                lines.push(format!("{workload} {field}: {old} -> {new}"));
            }
        }
    }
    Ok(lines)
}

async fn optimization_fixtures() -> Result<Vec<Row>> {
    let mut rows = Vec::new();
    for domain in ["s8", "u8", "s16", "u16", "s32", "u32", "s64", "u64"] {
        let wide = domain.ends_with("64");
        let source = if wide {
            include_str!("fixtures/optimization/add_bigint.ts")
        } else {
            include_str!("fixtures/optimization/add.ts")
        };
        let compiled = component(
            source,
            &format!(
                "package test:integers; world test {{ export add: func(x: {domain}, y: {domain}) -> {domain}; }}"
            ),
            "test",
        )?;
        rows.push(measure_core(
            &format!("WIT {domain} addition"),
            &compiled,
            "add",
            if wide {
                &[Val::I64(20), Val::I64(22)]
            } else {
                &[Val::I32(20), Val::I32(22)]
            },
            if wide { Val::I64(42) } else { Val::I32(42) },
        )?);
    }
    let widening = component(
        include_str!("fixtures/optimization/add.ts"),
        "package test:integers; world test { export add: func(x: u8, y: u8) -> u32; }",
        "test",
    )?;
    rows.push(measure_core(
        "WIT backward result width",
        &widening,
        "add",
        &[Val::I32(255), Val::I32(255)],
        Val::I32(510),
    )?);
    let shared = component(
        include_str!("fixtures/optimization/shared.ts"),
        "package test:integers; world test {
          export narrow: func(x: u8, y: u8) -> u8;
          export wide: func(x: u8, y: u8) -> u32;
          export floating: func(x: f64, y: f64) -> f64;
        }",
        "test",
    )?;
    rows.push(measure_core(
        "WIT shared helper",
        &shared,
        "wide",
        &[Val::I32(255), Val::I32(255)],
        Val::I32(510),
    )?);
    let recursive = component(
        include_str!("fixtures/optimization/recursive.ts"),
        "package test:integers; world test { export sum: func(n: u32) -> u32; }",
        "test",
    )?;
    rows.push(measure_core(
        "WIT recursion",
        &recursive,
        "sum",
        &[Val::I32(10)],
        Val::I32(55),
    )?);
    let unwrap = component(
        include_str!("fixtures/optimization/unwrap.ts"),
        "package test:optimization; world test { export run: func(n: f64) -> f64; }",
        "test",
    )?;
    rows.push(measure_core(
        "successful unwrap reuse",
        &unwrap,
        "run",
        &[Val::F64(4.0f64.to_bits())],
        Val::F64(12.0f64.to_bits()),
    )?);
    let asynchronous = component(
        include_str!("fixtures/optimization/async.ts"),
        "package test:optimization; world test { import wasi:cli/stderr@0.3.0; export run: async func(n: f64) -> f64; }",
        "test",
    )?;
    rows.push(
        measure(
            "async frame liveness",
            &asynchronous,
            None,
            |mut store, instance| async move {
                let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
                let (times, used, first_used) = timed!(store, SAMPLES, {
                    let (result,) = run.call_async(&mut store, (4.0,)).await?;
                    ensure!(result == 17.0, "async frame result");
                });
                Ok((store, times, used, first_used))
            },
        )
        .await?,
    );
    Ok(rows)
}

fn measure_core(
    workload: &str,
    compiled: &perry_wit::Compiled,
    export: &str,
    args: &[Val],
    expected: Val,
) -> Result<Row> {
    let engine = engine(true)?;
    let module = wasmtime::Module::new(&engine, &compiled.core)?;
    let mut store = Store::new(&engine, Memory::default());
    store.limiter(|memory| memory);
    store.set_fuel(u64::MAX)?;
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    let run = instance
        .get_func(&mut store, export)
        .expect("fixture export");
    let mut results = [Val::I32(0)];
    let before = store.get_fuel()?;
    let start = Instant::now();
    run.call(&mut store, args, &mut results)?;
    let milliseconds = start.elapsed().as_secs_f64() * 1000.0;
    let fuel = before - store.get_fuel()?;
    let correct = match (results[0], expected) {
        (Val::I32(actual), Val::I32(expected)) => actual == expected,
        (Val::I64(actual), Val::I64(expected)) => actual == expected,
        (Val::F64(actual), Val::F64(expected)) => actual == expected,
        _ => false,
    };
    ensure!(correct, "{workload}: wrong result {:?}", results[0]);
    Ok(Row {
        workload: workload.into(),
        bytes: Some(
            compiled
                .component
                .as_ref()
                .expect("fixture component")
                .stripped
                .len(),
        ),
        core_bytes: Some(compiled.core.len()),
        defined_functions: Some(defined_functions(&compiled.core)?),
        helper_functions: Some(helper_functions(&compiled.allocation_report)?),
        optimization: Some(optimization_statistics(&compiled.allocation_report)?),
        initialization_fuel: Some(u64::MAX - before),
        invocation_fuel: Some(fuel),
        initial_memory: Some(store.data().initial.unwrap_or(0)),
        memory: Some(store.data().bytes),
        milliseconds: Some(milliseconds),
        ..Row::default()
    })
}

#[test]
fn timed_calls_account_for_warmup_separately() -> Result<()> {
    let engine = engine(true)?;
    let module = wasmtime::Module::new(
        &engine,
        wat::parse_str(
            r#"(module
              (global $calls (mut i32) (i32.const 0))
              (func $start nop)
              (start $start)
              (func (export "run") (result i32)
                global.get $calls i32.const 1 i32.add global.set $calls
                global.get $calls))"#,
        )?,
    )?;
    let mut store = store(&engine, None, true)?;
    let instance = wasmtime::Instance::new(&mut store, &module, &[])?;
    let before = store.get_fuel()?;
    ensure!(before < u64::MAX, "start function must consume fuel");
    let run = instance.get_typed_func::<(), i32>(&mut store, "run")?;
    let mut results = Vec::new();
    let (times, warmed, first) = timed!(store, 2, {
        results.push(run.call(&mut store, ())?);
    });
    assert_eq!(results, [1, 2, 3, 4]);
    assert_eq!(times.len(), 2);
    assert!(first > 0);
    assert_eq!(warmed, first);
    assert_eq!(spent(&store, before), 4 * first);
    Ok(())
}
