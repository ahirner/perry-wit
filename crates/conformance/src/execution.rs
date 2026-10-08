//! Isolated effectful adapters. Models and observation checks do not perform I/O.
use crate::{
    program::{FIXTURE, INPUTS, WIT},
    registry::Case,
};
use anyhow::{Context, Result, ensure};
use perry_wit::{
    CompileOptions, compile_typescript,
    waffle_backend::{WaffleCompileOptions, compile_typescript_for_world},
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub fuel: u64,
    pub memory_bytes: usize,
    pub timeout_seconds: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            fuel: 100_000_000,
            memory_bytes: 8 * 1024 * 1024,
            timeout_seconds: 30,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub value: String,
    pub trace: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "stage", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Values {
        observations: Vec<Observation>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        max_fuel_used: u64,
        component_bytes: usize,
    },
    Command(Captured),
    Compile(String),
    Validate(String),
    Execute(String),
    Fuel(String),
    Crash(String),
    Timeout,
}
impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Values {
                observations,
                stdout,
                stderr,
                max_fuel_used,
                component_bytes,
            } => f
                .debug_struct("Values")
                .field("observations", &observations.len())
                .field(
                    "sample",
                    &observations.first().map(|o| {
                        (
                            &o.value,
                            o.trace.len(),
                            o.trace.iter().take(8).collect::<Vec<_>>(),
                        )
                    }),
                )
                .field("stdout_bytes", &stdout.len())
                .field("stderr_bytes", &stderr.len())
                .field("max_fuel_used", max_fuel_used)
                .field("component_bytes", component_bytes)
                .finish(),
            Self::Command(value) => f
                .debug_struct("Command")
                .field("exit", &value.exit)
                .field("stdout_bytes", &value.stdout.len())
                .field("stderr_bytes", &value.stderr.len())
                .field("timed_out", &value.timed_out)
                .finish(),
            Self::Compile(s) => f.debug_tuple("Compile").field(s).finish(),
            Self::Validate(s) => f.debug_tuple("Validate").field(s).finish(),
            Self::Execute(s) => f.debug_tuple("Execute").field(s).finish(),
            Self::Fuel(s) => f.debug_tuple("Fuel").field(s).finish(),
            Self::Crash(s) => f.debug_tuple("Crash").field(s).finish(),
            Self::Timeout => f.write_str("Timeout"),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Captured {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}
impl Captured {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit == Some(0)
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub oracle: Outcome,
    pub component: Outcome,
    pub control: Option<Outcome>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fault {
    Value,
    Trace,
    Resource,
}
impl Fault {
    fn source(self, source: &str) -> String {
        match self {
            Self::Value => source.replace("value:value", "value:(value === 0 ? 1 : 0)"),
            Self::Trace => source.replace("mark(trace, ", "identity("),
            Self::Resource => source.into(),
        }
    }
}
pub fn check_fault(case: &Case, evidence: &Evidence) -> Result<()> {
    let Case::Fault { program, fault } = case else {
        anyhow::bail!("fault checker requires a fault case")
    };
    let control = evidence
        .control
        .clone()
        .context("fault case lacks unchanged component evidence")?;
    check_equivalence(
        &Case::Program(program.clone()),
        &Evidence {
            oracle: evidence.oracle.clone(),
            component: control,
            control: None,
        },
    )?;
    match (fault, &evidence.oracle, &evidence.component) {
        (Fault::Resource, _, Outcome::Execute(message)) => ensure!(
            message.contains("host resources leaked"),
            "wrong resource failure: {message}"
        ),
        (
            Fault::Value,
            Outcome::Values {
                observations: a, ..
            },
            Outcome::Values {
                observations: b, ..
            },
        ) => ensure!(
            a.iter().zip(b).any(|(a, b)| a.value != b.value),
            "value fault was missed"
        ),
        (
            Fault::Trace,
            Outcome::Values {
                observations: a, ..
            },
            Outcome::Values {
                observations: b, ..
            },
        ) => ensure!(
            a.iter().zip(b).any(|(a, b)| a.trace != b.trace),
            "trace fault was missed"
        ),
        _ => anyhow::bail!("injected fault produced the wrong failure: {evidence:?}"),
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub case: Case,
    pub limits: Limits,
}

pub fn check_metamorphic(evidence: &[Evidence]) -> Result<()> {
    if let Some(Evidence {
        oracle:
            Outcome::Values {
                observations: baseline,
                stdout,
                stderr,
                ..
            },
        ..
    }) = evidence.first()
    {
        for result in &evidence[1..] {
            if let Outcome::Values {
                observations,
                stdout: output,
                stderr: errors,
                ..
            } = &result.oracle
            {
                ensure!(
                    observations == baseline && output == stdout && errors == stderr,
                    "metamorphic form changed Node observations"
                );
            }
        }
    }
    Ok(())
}

pub fn check_equivalence(case: &Case, evidence: &Evidence) -> Result<()> {
    match (case, &evidence.oracle, &evidence.component) {
        (Case::Reject { diagnostic, .. }, _, Outcome::Compile(message)) => ensure!(
            !diagnostic.is_empty() && message.contains(diagnostic),
            "unexpected rejection: {message}"
        ),
        (Case::Reject { .. }, _, _) => anyhow::bail!("expected compiler diagnostic: {evidence:?}"),
        (
            _,
            Outcome::Values {
                observations: a, ..
            },
            Outcome::Values {
                observations: b, ..
            },
        ) => {
            ensure!(a == b, "value or side-effect trace mismatch: {evidence:?}");
            ensure!(
                a.len()
                    == if matches!(case, Case::Stream(_) | Case::Fetch(_) | Case::Handler(_)) {
                        3
                    } else {
                        INPUTS.len()
                    },
                "missing boundary observations"
            );
            if let (
                Outcome::Values {
                    stdout: a,
                    stderr: ae,
                    ..
                },
                Outcome::Values {
                    stdout: b,
                    stderr: be,
                    ..
                },
            ) = (&evidence.oracle, &evidence.component)
            {
                ensure!(a == b && ae == be, "function output bytes differ");
            }
        }
        (Case::Command { exit, .. }, Outcome::Command(a), Outcome::Command(b)) => {
            ensure!(
                !a.timed_out
                    && !b.timed_out
                    && a.exit == Some(exit.code())
                    && b.exit == Some(exit.code()),
                "command did not produce its declared exit status: {evidence:?}"
            );
            ensure!(
                a.stdout == b.stdout && a.stderr == b.stderr,
                "output bytes differ: {evidence:?}"
            );
            ensure!(
                !a.stdout.windows(5).any(|v| v == b"_FAIL"),
                "fixture assertion failed"
            );
        }
        _ => anyhow::bail!("execution did not establish equivalence: {evidence:?}"),
    }
    Ok(())
}

/// Capture through files so a full pipe cannot deadlock a child; reap every timeout.
pub fn command(command: &mut Command, directory: &Path, limit: Duration) -> Result<Captured> {
    let logs = tempfile::Builder::new()
        .prefix("process-")
        .tempdir_in(directory)?
        .keep();
    fs::write(logs.join("command.txt"), format!("{command:?}"))?;
    let mut child = command
        .stdout(Stdio::from(File::create(logs.join("stdout"))?))
        .stderr(Stdio::from(File::create(logs.join("stderr"))?))
        .stdin(Stdio::null())
        .spawn()?;
    let start = Instant::now();
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    Ok(Captured {
        exit: status.code(),
        timed_out,
        stdout: fs::read(logs.join("stdout"))?,
        stderr: fs::read(logs.join("stderr"))?,
    })
}

pub struct Executor {
    executable: PathBuf,
    pub limits: Limits,
}
impl Executor {
    pub fn new(executable: PathBuf, limits: Limits) -> Self {
        Self { executable, limits }
    }
    pub fn check(&self, case: &Case, directory: &Path) -> Result<Vec<Evidence>> {
        let evidence = case
            .variants()
            .into_iter()
            .enumerate()
            .map(|(index, variant)| self.execute(&variant, &directory.join(index.to_string())))
            .collect::<Result<Vec<_>>>()?;
        check_metamorphic(&evidence)?;
        Ok(evidence)
    }
    pub fn execute(&self, case: &Case, directory: &Path) -> Result<Evidence> {
        self.execute_source(case, &case.source(), directory)
    }
    pub fn execute_source(&self, case: &Case, source: &str, directory: &Path) -> Result<Evidence> {
        fs::create_dir_all(directory)?;
        let path = directory.join("case.ts");
        fs::write(&path, source)?;
        environment(directory)?;
        fs::write(directory.join("input-case.json"), serde_json::to_vec(case)?)?;
        if !matches!(case, Case::Reject { .. }) {
            let checked = command(
                Command::new("tsc")
                    .current_dir(directory)
                    .args([
                        "--ignoreConfig",
                        "--noEmit",
                        "--strict",
                        "--target",
                        "es2022",
                        "--module",
                        "esnext",
                        "--skipLibCheck",
                    ])
                    .arg(&path)
                    .arg(directory.join("capabilities.d.ts")),
                directory,
                Duration::from_secs(60),
            )?;
            ensure!(
                checked.success(),
                "source failed TypeScript: {}",
                String::from_utf8_lossy(&checked.stdout)
            );
        }
        let oracle = if matches!(case, Case::Reject { .. }) {
            Outcome::Compile("expected source-domain rejection".into())
        } else if matches!(case, Case::Command { .. }) {
            Outcome::Command(command(
                Command::new("node")
                    .arg(&path)
                    .env("PERRY_CONFORMANCE", "fixture"),
                directory,
                Duration::from_secs(self.limits.timeout_seconds),
            )?)
        } else {
            let captured = command(
                Command::new("node")
                    .args([
                        "--disable-warning=ExperimentalWarning",
                        "--harmony-temporal",
                        "--experimental-strip-types",
                    ])
                    .arg(directory.join("oracle.mjs"))
                    .arg(&path)
                    .arg(directory.join("inputs.json"))
                    .arg(serde_json::to_string(FIXTURE)?)
                    .arg(directory.join("input-case.json"))
                    .arg(directory.join("node-observations.json")),
                directory,
                Duration::from_secs(self.limits.timeout_seconds),
            )?;
            if captured.timed_out {
                Outcome::Timeout
            } else if !captured.success() {
                Outcome::Execute(String::from_utf8_lossy(&captured.stderr).into_owned())
            } else {
                Outcome::Values {
                    observations: serde_json::from_slice(&fs::read(
                        directory.join("node-observations.json"),
                    )?)
                    .context("Node observations")?,
                    stdout: captured.stdout,
                    stderr: captured.stderr,
                    max_fuel_used: 0,
                    component_bytes: 0,
                }
            }
        };
        let request = directory.join("request.json");
        let destination = directory.join("outcome.json");
        fs::write(
            &request,
            serde_json::to_vec(&Request {
                case: case.clone(),
                limits: self.limits.clone(),
            })?,
        )?;
        let output = command(
            Command::new(&self.executable)
                .arg("worker")
                .arg(request)
                .arg(&destination),
            directory,
            Duration::from_secs(self.limits.timeout_seconds + 5),
        )?;
        let component = if output.timed_out {
            Outcome::Timeout
        } else if !output.success() {
            Outcome::Crash(String::from_utf8_lossy(&output.stderr).into_owned())
        } else {
            serde_json::from_slice(&fs::read(destination).context("worker produced no outcome")?)?
        };
        let control = if let Case::Fault { program, .. } = case {
            Some(
                self.execute_source(
                    &Case::Program(program.clone()),
                    source,
                    &directory.join("control"),
                )?
                .component,
            )
        } else {
            None
        };
        let evidence = Evidence {
            oracle,
            component,
            control,
        };
        fs::write(
            directory.join("evidence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        Ok(evidence)
    }
}

pub fn worker(request: &Path, destination: &Path) -> Result<()> {
    let Request { case, limits } = serde_json::from_slice(&fs::read(request)?)?;
    let directory = request.parent().context("worker directory")?;
    let path = directory.join("case.ts");
    let source = fs::read_to_string(&path)?;
    let outcome = match &case {
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
                    compiled
                        .stripped
                        .or(compiled.component)
                        .context("component output")?,
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
                            "-W",
                            "component-model-more-async-builtins=y",
                            "-W",
                            "component-model-async-stackful=y",
                            "-W",
                            "component-model-threading=y",
                            "--env",
                            "PERRY_CONFORMANCE=fixture",
                        ])
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
        _ => {
            let fault = if let Case::Fault { fault, .. } = &case {
                Some(*fault)
            } else {
                None
            };
            let source = fault.map_or(source.clone(), |fault| fault.source(&source));
            if fault.is_some() {
                fs::write(directory.join("injected.ts"), &source)?;
            }
            execute_function(&source, &path, &case, &limits, fault)
        }
    };
    fs::write(destination, serde_json::to_vec_pretty(&outcome)?)?;
    Ok(())
}

fn environment(directory: &Path) -> Result<()> {
    fs::write(directory.join("fixture.txt"), FIXTURE)?;
    fs::write(directory.join("world.wit"), WIT)?;
    fs::write(directory.join("async-world.wit"), async_wit())?;
    fs::write(
        directory.join("capabilities.d.ts"),
        "/// <reference path=\"./handler-types/runtime.d.ts\" />\ndeclare module \"test:generated/control\" { export function release():void; }",
    )?;
    perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../src/waffle_backend/http/handler"),
        world: Some("handler".into()),
        out_dir: directory.join("handler-types"),
        project_root: Some(directory.into()),
        entry: "case.ts".into(),
        initialize_tsconfig: false,
    })?;
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(
        directory.join("inputs.json"),
        serde_json::to_vec(&INPUTS.map(number))?,
    )?;
    Ok(())
}

fn async_wit() -> String {
    WIT.replace(
        "export run: func",
        "include wasi:cli/imports@0.3.0; export run: async func",
    )
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
            .map_err(|(map, error)| anyhow::anyhow!(error.render(&map)))?;
        let mut paths = fs::read_dir(env::var("WASI_P3_WIT_PATH")?)?
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

fn execute_function(
    source: &str,
    path: &Path,
    case: &Case,
    limits: &Limits,
    fault: Option<Fault>,
) -> Outcome {
    let directory = path.parent().unwrap();
    if matches!(case, Case::Stream(_)) {
        let wit = async_wit().replace("x: f64", "input: stream<u8>");
        if let Err(error) = fs::write(directory.join("async-world.wit"), wit) {
            return Outcome::Compile(error.to_string());
        }
    }
    if matches!(case, Case::Fetch(_)) {
        let wit = async_wit().replace("x: f64", "url: string").replace(
            "include wasi:cli/imports@0.3.0;",
            "include wasi:cli/imports@0.3.0; import wasi:http/client@0.3.0;",
        );
        if let Err(error) = fs::write(directory.join("async-world.wit"), wit) {
            return Outcome::Compile(error.to_string());
        }
    }
    if matches!(case, Case::Lifecycle(_)) {
        let wit = async_wit().replace(
            "world generated {",
            "interface control { release: func(); } world generated { import control;",
        );
        if let Err(error) = fs::write(directory.join("async-world.wit"), wit) {
            return Outcome::Compile(error.to_string());
        }
    }
    let (resolve, world) = match resolve_world(directory, case.asynchronous()) {
        Ok(world) => world,
        Err(error) => return Outcome::Compile(format!("{error:#}")),
    };
    let compilation = if matches!(case, Case::Handler(_)) {
        perry_wit::waffle_backend::compile_http_handler(
            source,
            &path.to_string_lossy(),
            &WaffleCompileOptions::default(),
            perry_wit::waffle_backend::HttpHandlerOptions {
                max_request_bytes: 65536,
                max_response_bytes: 65536,
            },
        )
    } else {
        compile_typescript_for_world(
            source,
            &path.to_string_lossy(),
            &WaffleCompileOptions::default(),
            resolve,
            world,
        )
    };
    let compiled = match compilation {
        Ok(compiled) => compiled,
        Err(error) => return Outcome::Compile(format!("{error:#}")),
    };
    let mut config = Config::new();
    config
        .consume_fuel(true)
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config).unwrap();
    let bytes = compiled.component.unwrap();
    let component_bytes = bytes.len();
    let component = match Component::new(&engine, bytes) {
        Ok(component) => component,
        Err(error) => return Outcome::Validate(format!("{error:#}")),
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    match runtime.block_on(observations(
        &engine, &component, directory, limits, fault, case,
    )) {
        Ok((observations, max_fuel_used, stdout, stderr)) => Outcome::Values {
            observations,
            stdout,
            stderr,
            max_fuel_used,
            component_bytes,
        },
        Err(error)
            if error.downcast_ref::<wasmtime::Trap>() == Some(&wasmtime::Trap::OutOfFuel) =>
        {
            Outcome::Fuel(format!("{error:#}"))
        }
        Err(error) => Outcome::Execute(format!("{error:#}")),
    }
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct GuestObservation {
    value: f64,
    trace: Vec<f64>,
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

async fn observations(
    engine: &Engine,
    component: &Component,
    artifacts: &Path,
    limits: &Limits,
    fault: Option<Fault>,
    case: &Case,
) -> Result<(Vec<Observation>, u64, Vec<u8>, Vec<u8>)> {
    let fuel = limits.fuel;
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("fixture.txt"),
        fs::read(artifacts.join("fixture.txt"))?,
    )?;
    let stdout = output_capture::MemoryOutput::new(32 * 1024 * 1024);
    let stderr = output_capture::MemoryOutput::new(32 * 1024 * 1024);
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
        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
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
    if let Case::Handler(case) = case {
        use http_body_util::{BodyExt, Full};
        use wasmtime::AsContextMut;
        let service = wasmtime_wasi_http::p3::bindings::Service::instantiate_async(
            &mut store, component, &linker,
        )
        .await?;
        let mut observations = Vec::new();
        let mut max_fuel_used = 0;
        for _ in 0..3 {
            store.set_fuel(fuel)?;
            let request = http::Request::builder()
                .method("PUT")
                .uri("https://example.test/contract?q=1")
                .header("x-value", "first")
                .header("x-value", "second")
                .body(Full::new(bytes::Bytes::from(case.bytes.clone())))?;
            let (request, completed) = wasmtime_wasi_http::p3::Request::from_http(
                wasmtime_wasi_http::default_hooks(),
                request,
            );
            let observation = store
                .run_concurrent(async |accessor| -> Result<Observation> {
                    let response = service
                        .handle(accessor, request)
                        .await?
                        .map_err(|error| anyhow::anyhow!("{error:?}"))?;
                    let response = accessor
                        .with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
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
            observations.push(observation);
            max_fuel_used = max_fuel_used.max(fuel - store.get_fuel()?);
            store.assert_concurrent_state_empty();
            ensure!(
                store.data().table.is_empty(),
                "host resources leaked after handler"
            );
        }
        return Ok((
            observations,
            max_fuel_used,
            stdout.contents().to_vec(),
            stderr.contents().to_vec(),
        ));
    }
    let instance = linker.instantiate_async(&mut store, component).await?;
    let mut values = Vec::new();
    let mut max_fuel_used = 0;
    if let Case::Stream(case) = case {
        let run = instance
            .get_typed_func::<(wasmtime::component::StreamReader<u8>,), (GuestObservation,)>(
                &mut store, "run",
            )?;
        for _ in 0..3 {
            store.set_fuel(fuel)?;
            let input = wasmtime::component::StreamReader::new(&mut store, case.bytes.clone())?;
            let (value,) = run.call_async(&mut store, (input,)).await?;
            max_fuel_used = max_fuel_used.max(fuel - store.get_fuel()?);
            store.assert_concurrent_state_empty();
            ensure!(
                store.data().table.is_empty(),
                "host resources leaked after stream"
            );
            values.push(Observation {
                value: number(value.value),
                trace: value.trace.into_iter().map(number).collect(),
            });
        }
        return Ok((
            values,
            max_fuel_used,
            stdout.contents().to_vec(),
            stderr.contents().to_vec(),
        ));
    }
    if let Case::Fetch(case) = case {
        let reply = case.clone();
        let server = http_fixture::HttpFixture::new(move |_| {
            if reply.truncated {
                http_fixture::Reply::TruncatedBody(reply.bytes.clone(), reply.bytes.len() + 1)
            } else {
                http_fixture::Reply::Bytes(reply.status, reply.bytes.clone())
            }
        });
        let url = format!("http://{}/contract", server.address);
        let run = instance.get_typed_func::<(&str,), (GuestObservation,)>(&mut store, "run")?;
        for _ in 0..3 {
            store.set_fuel(fuel)?;
            let (value,) = run.call_async(&mut store, (&url,)).await?;
            max_fuel_used = max_fuel_used.max(fuel - store.get_fuel()?);
            store.assert_concurrent_state_empty();
            ensure!(
                store.data().table.is_empty(),
                "host resources leaked after Fetch"
            );
            values.push(Observation {
                value: number(value.value),
                trace: value.trace.into_iter().map(number).collect(),
            });
        }
        let requests = server.requests.lock().unwrap();
        ensure!(
            requests.len() == 3
                && requests.iter().all(|r| r.method == "GET"
                    && r.target == "/contract"
                    && r.headers
                        .iter()
                        .any(|(n, v)| n == "x-contract" && v == "bounded")),
            "Fetch side effects differ from the model"
        );
        return Ok((
            values,
            max_fuel_used,
            stdout.contents().to_vec(),
            stderr.contents().to_vec(),
        ));
    }
    let run = instance.get_typed_func::<(f64,), (GuestObservation,)>(&mut store, "run")?;
    let mut values = Vec::new();
    let mut max_fuel_used = 0;
    // Reuse the instance to catch retained state and allocation lifetime errors.
    let inputs: Vec<String> = serde_json::from_slice(&fs::read(artifacts.join("inputs.json"))?)?;
    for encoded in inputs {
        let input = if encoded == "NaN" {
            f64::NAN
        } else {
            f64::from_bits(u64::from_str_radix(&encoded, 16)?)
        };
        store.set_fuel(fuel)?;
        let (value,) = run
            .call_async(&mut store, (input,))
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("input {input:?}"))?;
        max_fuel_used = max_fuel_used.max(fuel - store.get_fuel()?);
        if fault == Some(Fault::Resource) {
            store.data_mut().table.push(())?;
        }
        store.assert_concurrent_state_empty();
        ensure!(
            store.data().table.is_empty(),
            "host resources leaked after run"
        );
        values.push(Observation {
            value: number(value.value),
            trace: value.trace.into_iter().map(number).collect(),
        });
    }
    Ok((
        values,
        max_fuel_used,
        stdout.contents().to_vec(),
        stderr.contents().to_vec(),
    ))
}

pub fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else {
        format!("{:016x}", value.to_bits())
    }
}

#[path = "../../../tests/support/output_capture.rs"]
mod output_capture;

#[allow(dead_code)]
#[path = "../../../tests/support/http_fixture.rs"]
mod http_fixture;
