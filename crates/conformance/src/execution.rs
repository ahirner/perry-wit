//! Isolated effectful adapters. Models and observation checks do not perform I/O.
use crate::{
    program::{FIXTURE, Form, INPUTS, WIT},
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
            fuel: 1_000_000,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "stage", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Values {
        observations: Vec<Observation>,
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
            ensure!(a.len() == INPUTS.len(), "missing boundary observations");
        }
        (_, Outcome::Command(a), Outcome::Command(b)) => {
            ensure!(
                a.success() && b.success(),
                "positive command failed: {evidence:?}"
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
        let forms = match case {
            Case::Program(p) if p.form == Form::Direct => Form::ALL.to_vec(),
            _ => vec![Form::Direct],
        };
        let mut evidence = Vec::new();
        for (index, form) in forms.into_iter().enumerate() {
            let mut variant = case.clone();
            if let Case::Program(p) = &mut variant
                && p.form == Form::Direct
            {
                p.form = form;
            }
            let directory = directory.join(index.to_string());
            fs::create_dir_all(&directory)?;
            let result = self.execute(&variant, &directory)?;
            if let Some(Evidence {
                oracle:
                    Outcome::Values {
                        observations: baseline,
                        ..
                    },
                ..
            }) = evidence.first()
                && let Outcome::Values { observations, .. } = &result.oracle
            {
                ensure!(
                    observations == baseline,
                    "metamorphic form changed Node observations"
                );
            }
            evidence.push(result);
        }
        Ok(evidence)
    }
    pub fn execute(&self, case: &Case, directory: &Path) -> Result<Evidence> {
        fs::create_dir_all(directory)?;
        let path = directory.join("case.ts");
        fs::write(&path, case.source())?;
        environment(directory)?;
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
                        "--experimental-strip-types",
                    ])
                    .arg(directory.join("oracle.mjs"))
                    .arg(&path)
                    .arg(directory.join("inputs.json"))
                    .arg(serde_json::to_string(FIXTURE)?),
                directory,
                Duration::from_secs(self.limits.timeout_seconds),
            )?;
            if captured.timed_out {
                Outcome::Timeout
            } else if !captured.success() {
                Outcome::Execute(String::from_utf8_lossy(&captured.stderr).into_owned())
            } else {
                Outcome::Values {
                    observations: serde_json::from_slice(&captured.stdout)
                        .context("Node observations")?,
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
                self.execute(&Case::Program(program.clone()), &directory.join("control"))?
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
    let outcome = match &case {
        Case::Command { source } | Case::Reject { source, .. } => match compile_typescript(
            source,
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
            let source = fault.map_or_else(|| case.source(), |fault| fault.source(&case.source()));
            if fault.is_some() {
                fs::write(directory.join("injected.ts"), &source)?;
            }
            execute_function(&source, &path, case.asynchronous(), &limits, fault)
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
        include_str!("../../../types/p3.d.ts"),
    )?;
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
    asynchronous: bool,
    limits: &Limits,
    fault: Option<Fault>,
) -> Outcome {
    let directory = path.parent().unwrap();
    let (resolve, world) = match resolve_world(directory, asynchronous) {
        Ok(world) => world,
        Err(error) => return Outcome::Compile(format!("{error:#}")),
    };
    let compiled = match compile_typescript_for_world(
        source,
        &path.to_string_lossy(),
        &WaffleCompileOptions::default(),
        resolve,
        world,
    ) {
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
    match runtime.block_on(observations(&engine, &component, directory, limits, fault)) {
        Ok((observations, max_fuel_used)) => Outcome::Values {
            observations,
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

async fn observations(
    engine: &Engine,
    component: &Component,
    artifacts: &Path,
    limits: &Limits,
    fault: Option<Fault>,
) -> Result<(Vec<Observation>, u64)> {
    let fuel = limits.fuel;
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("fixture.txt"),
        fs::read(artifacts.join("fixture.txt"))?,
    )?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        engine,
        Host {
            context,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    store.set_fuel(fuel)?;
    let mut linker = Linker::new(engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, component).await?;
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
    Ok((values, max_fuel_used))
}

pub fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else {
        format!("{:016x}", value.to_bits())
    }
}
