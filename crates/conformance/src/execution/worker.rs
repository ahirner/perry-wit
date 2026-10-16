//! The isolated worker process: compiles one case and runs the component under wasmtime.
use super::{Fault, Limits, Observation, Outcome, Request, async_wit, command, number};
use crate::{
    fetch::FetchCase, handler::HandlerCase, program::ENV_FIXTURE, registry::Case,
    streams::StreamCase,
};
use anyhow::{Context, Result, anyhow, ensure};
use http_body_util::{BodyExt, Full};
use perry_wit::{CompileOptions, compile_for_world, compile_typescript};
use std::{env, fs, path::Path, process::Command, sync::Arc, time::Duration};
use wasmtime::component::{Component, Instance, Linker, ResourceTable, StreamReader};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// Each component's stdout and stderr capture limit.
const OUTPUT_LIMIT: usize = 32 * 1024 * 1024;

pub fn worker(request: &Path, destination: &Path) -> Result<()> {
    let Request { case, limits } = serde_json::from_slice(&fs::read(request)?)?;
    let directory = request.parent().context("worker directory")?;
    let path = directory.join("case.ts");
    let source = fs::read_to_string(&path)?;
    let outcome = match &*case {
        Case::Command { .. } | Case::Reject { .. } => match compile_typescript(
            &source,
            &path.to_string_lossy(),
            &CompileOptions {
                world: Some("command".into()),
                ..Default::default()
            },
        ) {
            Err(error) => Outcome::Compile(format!("{error:#}")),
            Ok(compiled) => {
                let wasm = directory.join("case.wasm");
                fs::write(
                    &wasm,
                    compiled.component.context("component output")?.stripped,
                )?;
                Outcome::Command(command(
                    Command::new("wasmtime")
                        .args([
                            "run",
                            "-C",
                            "cache=n",
                            "-S",
                            "p3=y",
                            "-W",
                            "component-model-async=y",
                            "--env",
                        ])
                        .arg(format!("PERRY_CONFORMANCE={ENV_FIXTURE}"))
                        .arg("-W")
                        .arg(format!("fuel={}", limits.fuel))
                        .arg("-W")
                        .arg(format!("max-memory-size={}", limits.memory_bytes))
                        .arg("-W")
                        .arg(format!("timeout={}s", limits.timeout_seconds))
                        .arg(wasm),
                    directory,
                    Duration::from_secs(limits.timeout_seconds),
                )?)
            }
        },
        case => {
            let fault = match case {
                Case::Fault { fault, .. } => Some(*fault),
                _ => None,
            };
            let source = match fault {
                Some(fault) => {
                    let injected = fault.source(source);
                    fs::write(directory.join("injected.ts"), &injected)?;
                    injected
                }
                None => source,
            };
            execute_function(&source, directory, &path, case, &limits, fault)?
        }
    };
    fs::write(destination, serde_json::to_vec_pretty(&outcome)?)?;
    Ok(())
}

fn resolve_world(
    directory: &Path,
    asynchronous: bool,
) -> Result<(wit_parser::Resolve, wit_parser::WorldId)> {
    let mut resolve = wit_parser::Resolve::default();
    let path = directory.join(if asynchronous {
        "async-world.wit"
    } else {
        "world.wit"
    });
    let wit = fs::read_to_string(path)?;
    let package = if asynchronous {
        let main = wit_parser::UnresolvedPackageGroup::parse("generated.wit", &wit)
            .map_err(|(map, error)| anyhow!(error.render(&map)))?;
        let mut paths = fs::read_dir(env::var("WASI_WIT_PATH")?)?
            .map(|entry| Ok(entry?.path()))
            .collect::<Result<Vec<_>>>()?;
        paths.sort();
        let dependencies = paths
            .into_iter()
            .map(wit_parser::UnresolvedPackageGroup::parse_dir)
            .collect::<Result<Vec<_>>>()?;
        resolve.push_groups(main, dependencies)?
    } else {
        resolve.push_str("generated.wit", &wit)?
    };
    let world = resolve.select_world(&[package], Some("generated"))?;
    Ok((resolve, world))
}

/// Compiles and runs a function case; failures of the case itself become outcomes, and only
/// host setup failures are errors.
fn execute_function(
    source: &str,
    directory: &Path,
    path: &Path,
    case: &Case,
    limits: &Limits,
    fault: Option<Fault>,
) -> Result<Outcome> {
    if let Case::Aggregate(agg) = case
        && let Err(error) = fs::write(directory.join("world.wit"), agg.wit_world())
    {
        return Ok(Outcome::Compile(error.to_string()));
    }
    let world = match case {
        Case::Stream(_) => Some(async_wit().replace("x: f64", "input: stream<u8>")),
        Case::Fetch(_) => Some(async_wit().replace("x: f64", "url: string").replace(
            "include wasi:cli/imports@0.3.0;",
            "include wasi:cli/imports@0.3.0; import wasi:http/client@0.3.0;",
        )),
        Case::Lifecycle(_) => Some(async_wit().replace(
            "world generated {",
            "interface control { release: func(); } world generated { import control;",
        )),
        _ => None,
    };
    if let Some(wit) = world
        && let Err(error) = fs::write(directory.join("async-world.wit"), wit)
    {
        return Ok(Outcome::Compile(error.to_string()));
    }
    let (resolve, world) = match resolve_world(directory, case.asynchronous()) {
        Ok(world) => world,
        Err(error) => return Ok(Outcome::Compile(format!("{error:#}"))),
    };
    let compile_one = || {
        if matches!(case, Case::Handler(_)) {
            // A `perry:http-handler` program compiles to a `wasi:http/handler` component.
            perry_wit::component::wit::resolve_wit(
                &Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/handler"),
            )
            .and_then(|(resolve, package)| {
                let world = resolve.select_world(&[package], Some("handler"))?;
                Ok(compile_for_world(
                    source,
                    &path.to_string_lossy(),
                    &resolve,
                    world,
                    true,
                )?)
            })
        } else {
            compile_for_world(source, &path.to_string_lossy(), &resolve, world, true)
                .map_err(anyhow::Error::from)
        }
    };
    let compiled = match compile_one() {
        Ok(compiled) => compiled,
        Err(error) => return Ok(Outcome::Compile(format!("{error:#}"))),
    };
    let summarized_bytes = if fault.is_none() {
        let summarized = match perry_mir::with_fact_limits(1, 1, compile_one) {
            Ok(compiled) => compiled,
            Err(error) => {
                return Ok(Outcome::Compile(format!(
                    "forced summarization compilation failed: {error:#}"
                )));
            }
        };
        Some(
            summarized
                .component
                .context("summarized component compilation produced no component")?
                .bytes,
        )
    } else {
        None
    };
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .wasm_component_model_async(true)
        .wasm_component_model_async_stackful(false)
        .wasm_component_model_more_async_builtins(false)
        .wasm_component_model_threading(false);
    let engine = Engine::new(&config)?;
    let bytes = compiled
        .component
        .context("component compilation produced no component")?
        .bytes;
    let component_bytes = bytes.len();
    let need_summarized_run = summarized_bytes.as_ref().is_some_and(|sum_bytes| {
        sum_bytes != &bytes || matches!(case, Case::Aggregate(_) | Case::Collection(_))
    });
    let component = match Component::new(&engine, bytes) {
        Ok(component) => component,
        Err(error) => return Ok(Outcome::Validate(format!("{error:#}"))),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let run = match runtime.block_on(observations(
        &engine, &component, directory, limits, fault, case,
    )) {
        Ok(run) => run,
        Err(error)
            if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) =>
        {
            return Ok(Outcome::Fuel(format!("{error:#}")));
        }
        Err(error) => return Ok(Outcome::Execute(format!("{error:#}"))),
    };
    if need_summarized_run && let Some(sum_bytes) = summarized_bytes {
        let sum_component = match Component::new(&engine, sum_bytes) {
            Ok(component) => component,
            Err(error) => {
                return Ok(Outcome::Validate(format!(
                    "forced summarization component failed validation: {error:#}"
                )));
            }
        };
        let sum_run = match runtime.block_on(observations(
            &engine,
            &sum_component,
            directory,
            limits,
            fault,
            case,
        )) {
            Ok(run) => run,
            Err(error)
                if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) =>
            {
                return Ok(Outcome::Fuel(format!(
                    "forced summarization exhausted fuel: {error:#}"
                )));
            }
            Err(error) => {
                return Ok(Outcome::Execute(format!(
                    "forced summarization execution failed: {error:#}"
                )));
            }
        };
        if sum_run.observations != run.observations
            || sum_run.stdout != run.stdout
            || sum_run.stderr != run.stderr
        {
            return Ok(Outcome::Execute(
                "forced summarization (MAX_ORIGINS=1, MAX_PRECISE_CONTEXTS=1) changed component observations"
                    .into(),
            ));
        }
    }
    let Run {
        observations,
        fuel_used,
        stdout,
        stderr,
    } = run;
    Ok(Outcome::Values {
        observations,
        stdout,
        stderr,
        max_fuel_used: fuel_used.iter().copied().max().unwrap_or(0),
        component_bytes,
        fuel_used,
    })
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct GuestObservation {
    value: f64,
    trace: Vec<f64>,
}

impl From<GuestObservation> for Observation {
    fn from(guest: GuestObservation) -> Self {
        Self {
            value: number(guest.value),
            trace: guest.trace.into_iter().map(number).collect(),
        }
    }
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct GuestMultiObservation {
    value: f64,
    head: f64,
    tail: f64,
    trace: Vec<f64>,
}

impl From<GuestMultiObservation> for Observation {
    fn from(mut guest: GuestMultiObservation) -> Self {
        guest.trace.push(guest.head + 0.0);
        guest.trace.push(guest.tail + 0.0);
        Self {
            value: number(guest.value),
            trace: guest.trace.into_iter().map(number).collect(),
        }
    }
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct GuestOptionObservation {
    value: f64,
    maybe: Option<f64>,
    trace: Vec<f64>,
}

impl From<GuestOptionObservation> for Observation {
    fn from(mut guest: GuestOptionObservation) -> Self {
        guest
            .trace
            .push(guest.maybe.map(|v| v + 0.0).unwrap_or(-66.0));
        Self {
            value: number(guest.value),
            trace: guest.trace.into_iter().map(number).collect(),
        }
    }
}

struct Host {
    context: WasiCtx,
    http: wasmtime_wasi_http::WasiHttpCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

impl wasmtime_wasi_http::WasiHttpView for Host {
    fn http(&mut self) -> wasmtime_wasi_http::WasiHttpCtxView<'_> {
        wasmtime_wasi_http::WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

struct Run {
    observations: Vec<Observation>,
    fuel_used: Vec<u64>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Observations and per-call fuel, in call order.
type Calls = (Vec<Observation>, Vec<u64>);

async fn observations(
    engine: &Engine,
    component: &Component,
    artifacts: &Path,
    limits: &Limits,
    fault: Option<Fault>,
    case: &Case,
) -> Result<Run> {
    let fuel = limits.fuel;
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("fixture.txt"),
        fs::read(artifacts.join("fixture.txt"))?,
    )?;
    let stdout = output_capture::MemoryOutput::new(OUTPUT_LIMIT);
    let stderr = output_capture::MemoryOutput::new(OUTPUT_LIMIT);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .preopened_dir(directory.path(), "/", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        engine,
        Host {
            context,
            http: wasmtime_wasi_http::WasiHttpCtx::new(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(if matches!(case, Case::Stream(_)) {
                    limits.memory_bytes.min(65536)
                } else {
                    limits.memory_bytes
                })
                .build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    store.set_fuel(fuel)?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    if matches!(case, Case::Fetch(_) | Case::Handler(_)) {
        wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    }
    if matches!(case, Case::Lifecycle(_)) {
        let gate = Arc::new(tokio::sync::Notify::new());
        linker.allow_shadowing(true);
        let wait_gate = gate.clone();
        linker
            .instance("wasi:clocks/monotonic-clock@0.3.0")?
            .func_wrap_concurrent("wait-for", move |_, (_duration,): (u64,)| {
                let gate = wait_gate.clone();
                Box::pin(async move {
                    gate.notified().await;
                    Ok(())
                })
            })?;
        linker.instance("test:generated/control")?.func_wrap(
            "release",
            move |_: wasmtime::StoreContextMut<'_, Host>, (): ()| {
                gate.notify_one();
                Ok(())
            },
        )?;
    }
    let (observations, fuel_used) = if let Case::Handler(case) = case {
        handler_calls(&mut store, component, &linker, case, fuel).await?
    } else {
        let instance = linker.instantiate_async(&mut store, component).await?;
        match case {
            Case::Stream(case) => stream_calls(&mut store, &instance, case, fuel).await?,
            Case::Fetch(case) => fetch_calls(&mut store, &instance, case, fuel).await?,
            _ => number_calls(&mut store, &instance, artifacts, fault, fuel, case).await?,
        }
    };
    Ok(Run {
        observations,
        fuel_used,
        stdout: stdout.contents().into(),
        stderr: stderr.contents().into(),
    })
}

/// Three exchanges with one `wasi:http/handler` instance.
async fn handler_calls(
    store: &mut Store<Host>,
    component: &Component,
    linker: &Linker<Host>,
    case: &HandlerCase,
    fuel: u64,
) -> Result<Calls> {
    let service = wasmtime_wasi_http::p3::bindings::Service::instantiate_async(
        &mut *store,
        component,
        linker,
    )
    .await?;
    let body = bytes::Bytes::from(case.bytes.clone());
    let mut calls = Calls::default();
    for _ in 0..3 {
        store.set_fuel(fuel)?;
        let request = http::Request::builder()
            .method("PUT")
            .uri("https://example.test/contract?q=1")
            .header("x-value", "first")
            .header("x-value", "second")
            .body(Full::new(body.clone()))?;
        let (request, completed) = wasmtime_wasi_http::p3::Request::from_http(
            wasmtime_wasi_http::default_hooks(),
            request,
        );
        let observation = store
            .run_concurrent(async |accessor| -> Result<Observation> {
                let response = service
                    .handle(accessor, request)
                    .await?
                    .map_err(|error| anyhow!("{error:?}"))?;
                let response =
                    accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
                ensure!(
                    response
                        .headers()
                        .get_all("x-value")
                        .iter()
                        .map(|v| v.as_bytes())
                        .collect::<Vec<_>>()
                        == [b"first".as_slice(), b"second".as_slice()],
                    "native handler lost duplicate headers"
                );
                let status = response.status().as_u16();
                let bytes = response.into_body().collect().await?.to_bytes();
                completed.await?;
                while accessor
                    .with(|mut store| store.as_context_mut().concurrent_state_table_size())
                    != 0
                {
                    tokio::task::yield_now().await;
                }
                Ok(Observation {
                    value: number(f64::from(status)),
                    trace: bytes.iter().map(|b| number(f64::from(*b))).collect(),
                })
            })
            .await??;
        calls.0.push(observation);
        calls.1.push(settle(store, fuel, "handler")?);
    }
    Ok(calls)
}

/// Three calls passing the case bytes as an owned WIT stream.
async fn stream_calls(
    store: &mut Store<Host>,
    instance: &Instance,
    case: &StreamCase,
    fuel: u64,
) -> Result<Calls> {
    let run = instance.get_typed_func::<(StreamReader<u8>,), (Result<GuestObservation, ()>,)>(
        &mut *store,
        "run",
    )?;
    let mut calls = Calls::default();
    for _ in 0..3 {
        store.set_fuel(fuel)?;
        let input = StreamReader::new(&mut *store, case.bytes.clone())?;
        let (value,) = run.call_async(&mut *store, (input,)).await?;
        calls.1.push(settle(store, fuel, "stream")?);
        calls.0.push(
            value
                .map_err(|()| anyhow!("case failed through its declared WIT result error"))?
                .into(),
        );
    }
    Ok(calls)
}

/// Three calls fetching from a loopback server that replies with the case body.
async fn fetch_calls(
    store: &mut Store<Host>,
    instance: &Instance,
    case: &FetchCase,
    fuel: u64,
) -> Result<Calls> {
    let reply = case.clone();
    let server = http_fixture::HttpFixture::new(move |_| {
        if reply.truncated {
            http_fixture::Reply::TruncatedBody(reply.bytes.clone(), reply.bytes.len() + 1)
        } else {
            http_fixture::Reply::Bytes(reply.status, reply.bytes.clone())
        }
    });
    let url = format!("http://{}/contract", server.address);
    let run =
        instance.get_typed_func::<(&str,), (Result<GuestObservation, ()>,)>(&mut *store, "run")?;
    let mut calls = Calls::default();
    for _ in 0..3 {
        store.set_fuel(fuel)?;
        let (value,) = run.call_async(&mut *store, (&url,)).await?;
        calls.1.push(settle(store, fuel, "Fetch")?);
        calls.0.push(
            value
                .map_err(|()| anyhow!("case failed through its declared WIT result error"))?
                .into(),
        );
    }
    let requests = server
        .requests
        .lock()
        .map_err(|_| anyhow!("Fetch fixture request log poisoned"))?;
    ensure!(
        requests.len() == 3
            && requests.iter().all(|r| r.method == "GET"
                && r.target == "/contract"
                && r.headers
                    .iter()
                    .any(|(n, v)| n == "x-contract" && v == "bounded")),
        "Fetch side effects differ from the model"
    );
    Ok(calls)
}

/// One call per boundary input, reusing the instance to catch retained state and allocation
/// lifetime errors.
async fn number_calls(
    store: &mut Store<Host>,
    instance: &Instance,
    artifacts: &Path,
    fault: Option<Fault>,
    fuel: u64,
    case: &Case,
) -> Result<Calls> {
    use crate::aggregates::WitSignature;
    let sig = match case {
        Case::Aggregate(agg) => agg.wit_signature,
        _ => WitSignature::ObservationRecord,
    };
    let inputs: Vec<String> = serde_json::from_slice(&fs::read(artifacts.join("inputs.json"))?)?;
    let mut calls = Calls::default();
    match sig {
        WitSignature::ObservationRecord => {
            let run = instance
                .get_typed_func::<(f64,), (Result<GuestObservation, ()>,)>(&mut *store, "run")?;
            for encoded in inputs {
                let input = decode_input(&encoded)?;
                store.set_fuel(fuel)?;
                let (value,) = run
                    .call_async(&mut *store, (input,))
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| format!("input {input:?}"))?;
                if fault == Some(Fault::Resource) {
                    store.data_mut().table.push(())?;
                }
                calls.1.push(settle(store, fuel, "run")?);
                calls.0.push(
                    value
                        .map_err(|()| anyhow!("case failed through its declared WIT result error"))?
                        .into(),
                );
            }
        }
        WitSignature::MultiFieldRecord => {
            let run = instance.get_typed_func::<(f64,), (Result<GuestMultiObservation, ()>,)>(
                &mut *store,
                "run",
            )?;
            for encoded in inputs {
                let input = decode_input(&encoded)?;
                store.set_fuel(fuel)?;
                let (value,) = run
                    .call_async(&mut *store, (input,))
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| format!("input {input:?}"))?;
                calls.1.push(settle(store, fuel, "run")?);
                calls.0.push(
                    value
                        .map_err(|()| anyhow!("case failed through its declared WIT result error"))?
                        .into(),
                );
            }
        }
        WitSignature::OptionRecord => {
            let run = instance.get_typed_func::<(f64,), (Result<GuestOptionObservation, ()>,)>(
                &mut *store,
                "run",
            )?;
            for encoded in inputs {
                let input = decode_input(&encoded)?;
                store.set_fuel(fuel)?;
                let (value,) = run
                    .call_async(&mut *store, (input,))
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| format!("input {input:?}"))?;
                calls.1.push(settle(store, fuel, "run")?);
                calls.0.push(
                    value
                        .map_err(|()| anyhow!("case failed through its declared WIT result error"))?
                        .into(),
                );
            }
        }
        WitSignature::DirectList => {
            let run =
                instance.get_typed_func::<(f64,), (Result<Vec<f64>, ()>,)>(&mut *store, "run")?;
            for encoded in inputs {
                let input = decode_input(&encoded)?;
                store.set_fuel(fuel)?;
                let (value,) = run
                    .call_async(&mut *store, (input,))
                    .await
                    .map_err(anyhow::Error::from)
                    .with_context(|| format!("input {input:?}"))?;
                calls.1.push(settle(store, fuel, "run")?);
                let list = value
                    .map_err(|()| anyhow!("case failed through its declared WIT result error"))?;
                let (first, rest) = list
                    .split_first()
                    .ok_or_else(|| anyhow!("direct list return was empty"))?;
                calls.0.push(Observation {
                    value: number(*first),
                    trace: rest.iter().copied().map(number).collect(),
                });
            }
        }
    }
    Ok(calls)
}

fn decode_input(encoded: &str) -> Result<f64> {
    if encoded == "NaN" {
        Ok(f64::NAN)
    } else {
        Ok(f64::from_bits(u64::from_str_radix(encoded, 16)?))
    }
}

/// Fuel used by the finished call; the call must leave no concurrent state or host resources.
fn settle(store: &mut Store<Host>, fuel: u64, call: &str) -> Result<u64> {
    let used = fuel - store.get_fuel()?;
    store.assert_concurrent_state_empty();
    ensure!(
        store.data().table.is_empty(),
        "host resources leaked after {call}"
    );
    Ok(used)
}

#[path = "../../../../tests/support/output_capture.rs"]
mod output_capture;

#[allow(dead_code)]
#[path = "../../../../tests/support/http_fixture.rs"]
mod http_fixture;
