//! Isolated execution and bounded reduction; a failed tool is never evidence of equivalence.
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail, ensure};
use perry_wit::waffle_backend::{WaffleCompileOptions, compile_typescript_for_world};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use super::model::{FIXTURE, Form, Generator, INPUTS, Number, Program, WIT};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub value: String,
    pub trace: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "stage", content = "detail")]
enum Outcome {
    Values {
        observations: Vec<Observation>,
        max_fuel_used: u64,
        component_bytes: usize,
    },
    Compile(String),
    Validate(String),
    Execute(String),
    Fuel(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum FailureKind {
    Compile,
    Validate,
    Execute,
    Fuel,
    Crash,
    Timeout,
    Value,
    Trace,
}

#[derive(Debug, Serialize)]
struct Failure {
    kind: FailureKind,
    detail: String,
}

#[derive(Debug)]
pub enum ProcessStatus {
    Exited(ExitStatus),
    TimedOut,
}

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: ProcessStatus,
    pub stdout: String,
    pub stderr: String,
}

impl ProcessOutput {
    pub fn success(&self) -> bool {
        matches!(self.status, ProcessStatus::Exited(status) if status.success())
    }
}

/// Files avoid pipe-buffer deadlocks; every child is reaped, including on timeout.
pub fn command(command: &mut Command, directory: &Path, limit: Duration) -> Result<ProcessOutput> {
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    let mut child = command
        .stdout(Stdio::from(File::create(&stdout)?))
        .stderr(Stdio::from(File::create(&stderr)?))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {command:?}; use nix develop for the test toolchain"))?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break ProcessStatus::Exited(status);
        }
        if start.elapsed() >= limit {
            // The process may have exited between try_wait and kill.
            let _ = child.kill();
            child.wait()?;
            break ProcessStatus::TimedOut;
        }
        thread::sleep(Duration::from_millis(10));
    };
    Ok(ProcessOutput {
        status,
        stdout: fs::read_to_string(stdout)?,
        stderr: fs::read_to_string(stderr)?,
    })
}

pub fn campaign() -> Result<()> {
    let compiler = CompilerWorker::new()?;
    let fuel = execution_fuel()?;
    let seed = setting("PERRY_GENERATIVE_SEED", 0_u64)?;
    let count = setting("PERRY_GENERATIVE_COUNT", 8_usize)?;
    let api_level = setting("PERRY_GENERATIVE_API_LEVEL", 2_u8)?;
    ensure!(api_level <= 5, "API level must be at most 5");
    let depth = setting("PERRY_GENERATIVE_DEPTH", 3_u8)?;
    ensure!((1..=10000).contains(&count), "count must be in 1..=10000");
    ensure!(depth <= 6, "depth must be at most 6");
    let shrink_limit = setting("PERRY_GENERATIVE_SHRINK", 100_usize)?;
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/generative");
    fs::create_dir_all(&root)?;
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)?
        .keep();
    eprintln!("Generative artifacts: {}", directory.display());
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (mut programs, forms) = if let Some(path) = env::var_os("PERRY_GENERATIVE_REPLAY") {
        let program = serde_json::from_slice::<Program>(&fs::read(path)?)?;
        let forms = vec![program.form];
        (vec![program], forms)
    } else {
        let mut forms = Form::ALL.to_vec();
        if api_level >= 3 {
            forms.extend([Form::Timer, Form::StoredTimer]);
        }
        if api_level >= 4 {
            forms.extend([Form::FileRead, Form::FileRoundTrip]);
        }
        if api_level >= 5 {
            forms.extend([Form::FileMetadata, Form::FileBytes]);
        }
        (
            serde_json::from_str::<Vec<Number>>(include_str!("regressions.json"))?
                .into_iter()
                .chain((0..count).map(|index| {
                    Generator::with_apis(seed.wrapping_add(index as u64), api_level).number(depth)
                }))
                .map(|expression| Program {
                    expression,
                    form: Form::Direct,
                })
                .collect::<Vec<_>>(),
            forms,
        )
    };
    for (case, program) in programs.iter_mut().enumerate() {
        for (variant, &form) in forms.iter().enumerate() {
            program.form = form;
            let index = case * forms.len() + variant;
            fs::write(directory.join(format!("case-{index}.ts")), program.source())?;
        }
    }
    fs::write(directory.join("world.wit"), WIT)?;
    fs::write(directory.join("async-world.wit"), async_wit())?;
    fs::write(
        directory.join("timers.d.ts"),
        "declare module 'node:timers/promises' { export function setTimeout<T>(delay: number, value: T): Promise<T>; }\ndeclare module 'node:fs/promises' { export function readFile(path: string, encoding: 'utf8'): Promise<string>; export function readFile(path: string): Promise<Uint8Array>; export function stat(path: string): Promise<{size:number;isFile():boolean;isDirectory():boolean}>; export function readdir(path: string): Promise<string[]>; export function writeFile(path: string, data: string): Promise<void>; }",
    )?;
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(
        directory.join("inputs.json"),
        serde_json::to_vec(&INPUTS.map(number))?,
    )?;
    let mut report = json!({
        "seed":seed, "count":programs.len() * forms.len(), "depth":depth,
        "completed":0, "status":"running", "startedUnix":started,
        "grammarVersion":6, "apiLevel":api_level, "inputsPerProgram":INPUTS.len(),
        "fuelPerCall":fuel,
        "replay":env::var_os("PERRY_GENERATIVE_REPLAY").is_some(),
    });
    let revision = Command::new("git").args(["rev-parse", "HEAD"]).output()?;
    ensure!(
        revision.status.success(),
        "cannot identify compiler revision"
    );
    report["revision"] = json!(String::from_utf8(revision.stdout)?.trim());
    let changes = Command::new("git")
        .args([
            "diff",
            "HEAD",
            "--",
            "Cargo.toml",
            "Cargo.lock",
            "flake.nix",
            "src",
            "tests/generative",
            "tests/generative_test.rs",
        ])
        .output()?;
    ensure!(
        changes.status.success(),
        "cannot record compiler and generator changes"
    );
    report["dirty"] = json!(!changes.stdout.is_empty());
    fs::write(directory.join("source.patch"), changes.stdout)?;
    fs::write(
        directory.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let mut tsc = Command::new("tsc");
    fs::write(
        directory.join("tsconfig.json"),
        r#"{"compilerOptions":{"noEmit":true,"strict":true,"target":"es2022","module":"esnext","skipLibCheck":true},"include":["case-*.ts","timers.d.ts"]}"#,
    )?;
    tsc.arg("-p").arg(directory.join("tsconfig.json"));
    let checked = command(&mut tsc, &directory, Duration::from_secs(60))?;
    ensure!(
        checked.success(),
        "generated source failed tsc: {checked:?}; artifacts: {}",
        directory.display()
    );
    let versions = json!({
        "node": Command::new("node").arg("--version").output().map(|v| String::from_utf8_lossy(&v.stdout).into_owned())?,
        "typescript": Command::new("tsc").arg("--version").output().map(|v| String::from_utf8_lossy(&v.stdout).into_owned())?,
        "compiler": env!("CARGO_PKG_VERSION"),
    });
    report["versions"] = versions;
    for (case, mut program) in programs.into_iter().enumerate() {
        let mut baseline = None;
        for (variant, &form) in forms.iter().enumerate() {
            program.form = form;
            let index = case * forms.len() + variant;
            let path = directory.join(format!("case-{index}.ts"));
            report["active"] = json!({"index":index,"form":program.form});
            fs::write(
                directory.join("report.json"),
                serde_json::to_vec_pretty(&report)?,
            )?;
            let expected = oracle(&path, &directory)?;
            if let Some(baseline) = &baseline {
                ensure!(
                    *baseline == expected,
                    "metamorphic transformation changed Node observations: {path:?}"
                );
            }
            if let Some(failure) =
                compiler.compare(&path, &directory, &expected, program.form.is_async())?
            {
                fs::write(directory.join("original.ts"), program.source())?;
                fs::write(
                    directory.join("original.json"),
                    serde_json::to_vec_pretty(&program)?,
                )?;
                fs::write(
                    directory.join("failure.json"),
                    serde_json::to_vec_pretty(&failure)?,
                )?;
                let (minimal, attempts) = reduce(program, shrink_limit, |candidate| {
                    let path = directory.join("candidate.ts");
                    fs::write(&path, candidate.source())?;
                    let expected = oracle(&path, &directory)?;
                    Ok(compiler
                        .compare(&path, &directory, &expected, candidate.form.is_async())?
                        .is_some_and(|next| next.kind == failure.kind))
                })?;
                let minimal_path = directory.join("minimal.ts");
                fs::write(&minimal_path, minimal.source())?;
                fs::write(
                    directory.join("minimal.json"),
                    serde_json::to_vec_pretty(&minimal)?,
                )?;
                let expected = oracle(&minimal_path, &directory)?;
                let reproduced = compiler
                    .compare(
                        &minimal_path,
                        &directory,
                        &expected,
                        minimal.form.is_async(),
                    )?
                    .context("reduction stopped reproducing")?;
                ensure!(
                    reproduced.kind == failure.kind,
                    "reduction changed failure category"
                );
                report["status"] = json!("failed");
                report["failure"] = serde_json::to_value(&reproduced)?;
                report["shrinkAttempts"] = json!(attempts);
                fs::write(
                    directory.join("report.json"),
                    serde_json::to_vec_pretty(&report)?,
                )?;
                bail!(
                    "{reproduced:?}\nArtifacts: {}\nReplay: PERRY_GENERATIVE_REPLAY={} cargo test --test generative_test generated_programs_match_node -- --nocapture",
                    directory.display(),
                    directory.join("minimal.json").display()
                );
            }
            report["completed"] = json!(index + 1);
            if baseline.is_none() {
                baseline = Some(expected);
            }
        }
    }
    report["elapsedSeconds"] =
        json!(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() - started);
    report["status"] = json!("passed");
    report["active"] = json!(null);
    fs::write(
        directory.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    eprintln!(
        "Passed {} programs × {} inputs; {}",
        report["completed"],
        INPUTS.len(),
        directory.display()
    );
    Ok(())
}

fn setting<T: std::str::FromStr>(name: &str, default: T) -> Result<T> {
    match env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid {name}: {value}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn oracle(path: &Path, directory: &Path) -> Result<Vec<Observation>> {
    let mut node = Command::new("node");
    node.args([
        "--disable-warning=ExperimentalWarning",
        "--experimental-strip-types",
    ])
    .arg(directory.join("oracle.mjs"))
    .arg(path)
    .arg(directory.join("inputs.json"))
    .arg(serde_json::to_string(FIXTURE)?);
    let output = command(&mut node, directory, Duration::from_secs(10))?;
    ensure!(
        output.success(),
        "Node oracle failed for {}: {output:?}",
        path.display()
    );
    serde_json::from_str(&output.stdout).context("Node observations")
}

/// Keep every child on the same executable even if another campaign rebuilds the tests.
struct CompilerWorker(tempfile::TempPath);

impl CompilerWorker {
    fn new() -> Result<Self> {
        let executable = tempfile::Builder::new()
            .prefix("perry-worker-")
            .suffix(std::env::consts::EXE_SUFFIX)
            .tempfile()?
            .into_temp_path();
        fs::copy(env::current_exe()?, &executable)?;
        Ok(Self(executable))
    }

    fn compare(
        &self,
        path: &Path,
        directory: &Path,
        expected: &[Observation],
        asynchronous: bool,
    ) -> Result<Option<Failure>> {
        let outcome = directory.join("outcome.json");
        if outcome.exists() {
            fs::remove_file(&outcome)?;
        }
        let mut worker = Command::new(&self.0);
        worker
            .args(["--exact", "generative_worker", "--ignored", "--nocapture"])
            .env("PERRY_GENERATIVE_SOURCE", path)
            .env("PERRY_GENERATIVE_OUTCOME", &outcome);
        if asynchronous {
            worker.env("PERRY_GENERATIVE_ASYNC", "1");
        } else {
            worker.env_remove("PERRY_GENERATIVE_ASYNC");
        }
        let output = command(&mut worker, directory, Duration::from_secs(30))?;
        let failure = if matches!(output.status, ProcessStatus::TimedOut) {
            Failure {
                kind: FailureKind::Timeout,
                detail: output.stderr,
            }
        } else if !output.success() {
            Failure {
                kind: FailureKind::Crash,
                detail: format!("{:?}\n{}\n{}", output.status, output.stdout, output.stderr),
            }
        } else {
            match serde_json::from_slice::<Outcome>(
                &fs::read(&outcome).context("worker produced no outcome")?,
            )? {
                Outcome::Compile(detail) => Failure {
                    kind: FailureKind::Compile,
                    detail,
                },
                Outcome::Validate(detail) => Failure {
                    kind: FailureKind::Validate,
                    detail,
                },
                Outcome::Execute(detail) => Failure {
                    kind: FailureKind::Execute,
                    detail,
                },
                Outcome::Fuel(detail) => Failure {
                    kind: FailureKind::Fuel,
                    detail,
                },
                Outcome::Values {
                    observations: actual,
                    ..
                } => {
                    if actual == expected {
                        return Ok(None);
                    }
                    let kind = if actual.len() == expected.len()
                        && actual.iter().zip(expected).all(|(a, b)| a.value == b.value)
                    {
                        FailureKind::Trace
                    } else {
                        FailureKind::Value
                    };
                    Failure {
                        kind,
                        detail: format!("expected {expected:?}; actual {actual:?}"),
                    }
                }
            }
        };
        Ok(Some(failure))
    }
}

/// Candidates must be strictly smaller; only the caller's original failure predicate is retained.
pub fn reduce(
    mut program: Program,
    limit: usize,
    mut reproduces: impl FnMut(&Program) -> Result<bool>,
) -> Result<(Program, usize)> {
    let mut attempts = 0;
    loop {
        let mut replacement = None;
        for candidate in program.reductions() {
            if attempts == limit {
                return Ok((program, attempts));
            }
            attempts += 1;
            if reproduces(&candidate)? {
                replacement = Some(candidate);
                break;
            }
        }
        match replacement {
            Some(candidate) => program = candidate,
            None => return Ok((program, attempts)),
        }
    }
}

pub fn worker() -> Result<()> {
    let path = PathBuf::from(
        env::var_os("PERRY_GENERATIVE_SOURCE")
            .context("worker is only run by the generative harness")?,
    );
    let destination =
        PathBuf::from(env::var_os("PERRY_GENERATIVE_OUTCOME").context("worker output")?);
    let source = fs::read_to_string(&path)?;
    let outcome = execute(
        &source,
        &path,
        env::var_os("PERRY_GENERATIVE_ASYNC").is_some(),
    );
    fs::write(destination, serde_json::to_vec(&outcome)?)?;
    Ok(())
}

fn async_wit() -> String {
    WIT.replace(
        "export run: func",
        "include wasi:cli/imports@0.3.0; export run: async func",
    )
}

fn resolve_world(asynchronous: bool) -> Result<(wit_parser::Resolve, wit_parser::WorldId)> {
    let mut resolve = wit_parser::Resolve::default();
    let package = if asynchronous {
        let main = wit_parser::UnresolvedPackageGroup::parse("generated.wit", &async_wit())
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
        resolve.push_str("generated.wit", WIT)?
    };
    let world = resolve.select_world(&[package], Some("generated"))?;
    Ok((resolve, world))
}

fn execute(source: &str, path: &Path, asynchronous: bool) -> Outcome {
    let (resolve, world) = match resolve_world(asynchronous) {
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
    match runtime.block_on(observations(&engine, &component)) {
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

fn execution_fuel() -> Result<u64> {
    let fuel = setting("PERRY_GENERATIVE_FUEL", 1_000_000_u64)?;
    ensure!(
        (1..=100_000_000).contains(&fuel),
        "fuel must be between 1 and 100000000"
    );
    Ok(fuel)
}

async fn observations(engine: &Engine, component: &Component) -> Result<(Vec<Observation>, u64)> {
    let fuel = execution_fuel()?;
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("fixture.txt"), FIXTURE)?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/", FsPerms::ReadWrite)?
        .build();
    let mut store = Store::new(
        engine,
        Host {
            context,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(8 * 1024 * 1024)
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
    for input in INPUTS {
        store.set_fuel(fuel)?;
        let (value,) = run
            .call_async(&mut store, (input,))
            .await
            .map_err(anyhow::Error::from)
            .with_context(|| format!("input {input:?}"))?;
        max_fuel_used = max_fuel_used.max(fuel - store.get_fuel()?);
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

/// Remove a real side effect from the compiled source to verify detection and reduction.
pub fn verify_trace_fault() -> Result<()> {
    let compiler = CompilerWorker::new()?;
    let directory = tempfile::tempdir()?;
    let directory = directory.path();
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(
        directory.join("inputs.json"),
        serde_json::to_vec(&INPUTS.map(number))?,
    )?;
    let original = Program {
        expression: Number::Call(Box::new(Number::Mark(Box::new(Number::Input)))),
        form: Form::Direct,
    };
    let mut detects = |candidate: &Program| -> Result<bool> {
        let source = candidate.source();
        let path = directory.join("reference.ts");
        fs::write(&path, &source)?;
        let expected = oracle(&path, directory)?;
        ensure!(
            compiler
                .compare(&path, directory, &expected, candidate.form.is_async())?
                .is_none(),
            "unmodified source must pass"
        );
        let path = directory.join("fault.ts");
        fs::write(&path, source.replace("mark(trace, ", "identity("))?;
        Ok(compiler
            .compare(&path, directory, &expected, candidate.form.is_async())?
            .is_some_and(|failure| failure.kind == FailureKind::Trace))
    };
    ensure!(detects(&original)?, "missing side effect was not detected");
    let (minimal, _) = reduce(original, 40, &mut detects)?;
    ensure!(
        minimal.expression == Number::Mark(Box::new(Number::Input)),
        "trace fault did not reduce: {minimal:?}"
    );
    ensure!(
        detects(&minimal)?,
        "reduced trace fault stopped reproducing"
    );
    Ok(())
}
