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
    input: Option<usize>,
}

impl Failure {
    fn preserves(&self, original: &Self) -> bool {
        self.kind == original.kind
            && match self.kind {
                FailureKind::Value | FailureKind::Trace => self.input == original.input,
                FailureKind::Compile
                | FailureKind::Validate
                | FailureKind::Execute
                | FailureKind::Fuel => self.detail == original.detail,
                FailureKind::Crash | FailureKind::Timeout => false,
            }
    }
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
    let directory = tempfile::Builder::new()
        .prefix("process-")
        .tempdir_in(directory)?
        .keep();
    fs::write(directory.join("command.txt"), format!("{command:?}"))?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    let mut child = command
        .stdout(Stdio::from(File::create(&stdout)?))
        .stderr(Stdio::from(File::create(&stderr)?))
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {command:?}; use nix develop for the test toolchain"))
        .inspect_err(|error| {
            let _ = fs::write(directory.join("status.txt"), format!("{error:#}"));
        })?;
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
    fs::write(directory.join("status.txt"), format!("{status:?}"))?;
    Ok(ProcessOutput {
        status,
        stdout: fs::read_to_string(stdout)?,
        stderr: fs::read_to_string(stderr)?,
    })
}

pub fn campaign() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/generative");
    fs::create_dir_all(&root)?;
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(root)?
        .keep();
    eprintln!("Generative artifacts: {}", directory.display());
    let started = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut report = json!({"completed":0, "status":"running", "startedUnix":started});
    save_report(&directory, &report)?;
    let start = Instant::now();
    let result = run_campaign(&directory, &mut report);
    report["elapsedSeconds"] = json!(start.elapsed().as_secs());
    match &result {
        Ok(()) => {
            report["status"] = json!("passed");
            report["active"] = json!(null);
            eprintln!(
                "Passed {} programs × {} inputs; {}",
                report["completed"],
                report["inputsPerProgram"],
                directory.display()
            );
        }
        Err(error) => {
            report["status"] = json!("failed");
            report["error"] = json!(format!("{error:#}"));
        }
    }
    if let Err(error) = save_report(&directory, &report) {
        if result.is_ok() {
            return Err(error);
        }
        eprintln!("Could not save campaign report: {error:#}");
    }
    result
}

fn save_report(directory: &Path, report: &serde_json::Value) -> Result<()> {
    let temporary = directory.join("report.json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(report)?)?;
    fs::rename(temporary, directory.join("report.json"))?;
    Ok(())
}

fn run_campaign(directory: &Path, report: &mut serde_json::Value) -> Result<()> {
    let compiler = CompilerWorker::new()?;
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
    save_report(directory, report)?;
    let versions = json!({
        "node": Command::new("node").arg("--version").output().map(|v| String::from_utf8_lossy(&v.stdout).into_owned())?,
        "typescript": Command::new("tsc").arg("--version").output().map(|v| String::from_utf8_lossy(&v.stdout).into_owned())?,
        "compiler": env!("CARGO_PKG_VERSION"),
    });
    report["versions"] = versions;
    if let Some(path) = env::var_os("PERRY_GENERATIVE_REPLAY") {
        return replay_saved(&PathBuf::from(path), directory, report, &compiler);
    }
    let fuel = execution_fuel()?;
    let seed = setting("PERRY_GENERATIVE_SEED", 0_u64)?;
    let count = setting("PERRY_GENERATIVE_COUNT", 8_usize)?;
    let api_level = setting("PERRY_GENERATIVE_API_LEVEL", 2_u8)?;
    ensure!(api_level <= 6, "API level must be at most 6");
    let depth = setting("PERRY_GENERATIVE_DEPTH", 3_u8)?;
    ensure!((1..=10000).contains(&count), "count must be in 1..=10000");
    ensure!(depth <= 6, "depth must be at most 6");
    let shrink_limit = setting("PERRY_GENERATIVE_SHRINK", 100_usize)?;
    let (mut programs, forms) = {
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
        if api_level >= 6 {
            forms.extend([Form::FileByteRoundTrip, Form::FileRejectedRead]);
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
            fs::write(
                directory.join(format!("case-{index}.json")),
                serde_json::to_vec(program)?,
            )?;
        }
    }
    save_environment(directory)?;
    report["seed"] = json!(seed);
    report["count"] = json!(programs.len() * forms.len());
    report["depth"] = json!(depth);
    report["grammarVersion"] = json!(8);
    report["apiLevel"] = json!(api_level);
    report["inputsPerProgram"] = json!(INPUTS.len());
    report["fuelPerCall"] = json!(fuel);
    report["replay"] = json!(false);
    save_report(directory, report)?;
    check_sources(directory)?;
    for (case, mut program) in programs.into_iter().enumerate() {
        let mut baseline = None;
        for (variant, &form) in forms.iter().enumerate() {
            program.form = form;
            let index = case * forms.len() + variant;
            let path = directory.join(format!("case-{index}.ts"));
            report["active"] = json!({"index":index,"form":program.form});
            save_report(directory, report)?;
            let expected = oracle(&path, directory)?;
            if let Some(baseline) = &baseline {
                ensure!(
                    *baseline == expected,
                    "metamorphic transformation changed Node observations: {path:?}"
                );
            }
            if let Some(failure) =
                compiler.compare(&path, directory, &expected, program.form.is_async())?
            {
                fs::copy(&path, directory.join("original.ts"))?;
                fs::write(
                    directory.join("original.json"),
                    serde_json::to_vec_pretty(&program)?,
                )?;
                fs::write(
                    directory.join("failure.json"),
                    serde_json::to_vec_pretty(&failure)?,
                )?;
                report["failure"] = serde_json::to_value(&failure)?;
                save_report(directory, report)?;
                let limit = if matches!(failure.kind, FailureKind::Crash | FailureKind::Timeout) {
                    0
                } else {
                    shrink_limit
                };
                let (minimal, attempts) = reduce(program, limit, |candidate| {
                    let path = directory.join("candidate.ts");
                    fs::write(&path, candidate.source())?;
                    let expected = oracle(&path, directory)?;
                    Ok(compiler
                        .compare(&path, directory, &expected, candidate.form.is_async())?
                        .is_some_and(|next| next.preserves(&failure)))
                })?;
                let minimal_path = directory.join("minimal.ts");
                fs::write(&minimal_path, minimal.source())?;
                fs::write(
                    directory.join("minimal.json"),
                    serde_json::to_vec_pretty(&minimal)?,
                )?;
                let expected = oracle(&minimal_path, directory)?;
                let reproduced = compiler
                    .compare(&minimal_path, directory, &expected, minimal.form.is_async())?
                    .context("reduction stopped reproducing")?;
                ensure!(
                    reproduced.preserves(&failure)
                        || (limit == 0
                            && matches!(failure.kind, FailureKind::Crash | FailureKind::Timeout)
                            && reproduced.kind == failure.kind),
                    "reduction changed failure signature"
                );
                report["reducedFailure"] = serde_json::to_value(&reproduced)?;
                report["shrinkAttempts"] = json!(attempts);
                save_report(directory, report)?;
                bail!(
                    "{reproduced:?}\nArtifacts: {}\nReplay: PERRY_GENERATIVE_REPLAY={} cargo test --test generative_test generated_programs_match_node -- --nocapture",
                    directory.display(),
                    directory.join("minimal.json").display()
                );
            }
            report["completed"] = json!(index + 1);
            save_report(directory, report)?;
            if baseline.is_none() {
                baseline = Some(expected);
            }
        }
    }
    Ok(())
}

fn save_environment(directory: &Path) -> Result<()> {
    fs::write(directory.join("fixture.txt"), FIXTURE)?;
    fs::write(directory.join("world.wit"), WIT)?;
    fs::write(directory.join("async-world.wit"), async_wit())?;
    fs::write(
        directory.join("timers.d.ts"),
        "declare module 'node:timers/promises' { export function setTimeout<T>(delay: number, value: T): Promise<T>; }\ndeclare module 'node:fs/promises' { export function readFile(path: string, encoding: 'utf8'): Promise<string>; export function readFile(path: string): Promise<Uint8Array>; export function stat(path: string): Promise<{size:number;isFile():boolean;isDirectory():boolean}>; export function readdir(path: string): Promise<string[]>; export function writeFile(path: string, data: string | Uint8Array): Promise<void>; }",
    )?;
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(
        directory.join("inputs.json"),
        serde_json::to_vec(&INPUTS.map(number))?,
    )?;
    Ok(())
}

fn check_sources(directory: &Path) -> Result<()> {
    let mut tsc = Command::new("tsc");
    fs::write(
        directory.join("tsconfig.json"),
        r#"{"compilerOptions":{"noEmit":true,"strict":true,"target":"es2022","module":"esnext","skipLibCheck":true},"include":["case-*.ts","timers.d.ts"]}"#,
    )?;
    tsc.arg("-p").arg(directory.join("tsconfig.json"));
    let checked = command(&mut tsc, directory, Duration::from_secs(60))?;
    ensure!(
        checked.success(),
        "generated source failed tsc: {checked:?}; artifacts: {}",
        directory.display()
    );
    Ok(())
}

fn replay_saved(
    path: &Path,
    directory: &Path,
    report: &mut serde_json::Value,
    compiler: &CompilerWorker,
) -> Result<()> {
    let source = path.with_extension("ts");
    let metadata: serde_json::Value =
        serde_json::from_slice(&fs::read(path.with_extension("json"))?)?;
    let form: Form = serde_json::from_value(metadata["form"].clone())?;
    let saved = path.parent().context("replay directory")?;
    let previous: serde_json::Value =
        serde_json::from_slice(&fs::read(saved.join("report.json"))?)?;
    for name in [
        "inputs.json",
        "fixture.txt",
        "oracle.mjs",
        "world.wit",
        "async-world.wit",
        "timers.d.ts",
    ] {
        fs::copy(saved.join(name), directory.join(name))
            .with_context(|| format!("replay requires saved {name}"))?;
    }
    let path = directory.join("case-0.ts");
    fs::copy(&source, &path)?;
    fs::copy(source.with_extension("json"), path.with_extension("json"))?;
    let inputs: Vec<String> = serde_json::from_slice(&fs::read(directory.join("inputs.json"))?)?;
    report["count"] = json!(1);
    report["replay"] = json!(true);
    report["replayedSource"] = json!(source);
    let fuel = previous["fuelPerCall"]
        .as_u64()
        .context("saved fuelPerCall must be an integer")?;
    ensure!(
        (1..=100_000_000).contains(&fuel),
        "saved fuel must be between 1 and 100000000"
    );
    report["fuelPerCall"] = json!(fuel);
    report["inputsPerProgram"] = json!(inputs.len());
    report["active"] = json!({"index":0,"form":form});
    save_report(directory, report)?;
    check_sources(directory)?;
    let expected = oracle(&path, directory)?;
    if let Some(failure) = compiler.compare(&path, directory, &expected, form.is_async())? {
        fs::write(
            directory.join("failure.json"),
            serde_json::to_vec_pretty(&failure)?,
        )?;
        report["failure"] = serde_json::to_value(&failure)?;
        bail!("{failure:?}; artifacts: {}", directory.display());
    }
    report["completed"] = json!(1);
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
    .arg(serde_json::to_string(&fs::read_to_string(
        directory.join("fixture.txt"),
    )?)?);
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
        if directory.join("report.json").exists() {
            let report: serde_json::Value =
                serde_json::from_slice(&fs::read(directory.join("report.json"))?)?;
            let fuel = report["fuelPerCall"]
                .as_u64()
                .context("saved fuelPerCall must be an integer")?;
            worker.env("PERRY_GENERATIVE_FUEL", fuel.to_string());
        }
        let output = command(&mut worker, directory, Duration::from_secs(30))?;
        let failure = if matches!(output.status, ProcessStatus::TimedOut) {
            Failure {
                kind: FailureKind::Timeout,
                input: None,
                detail: output.stderr,
            }
        } else if !output.success() {
            Failure {
                kind: FailureKind::Crash,
                input: None,
                detail: format!("{:?}\n{}\n{}", output.status, output.stdout, output.stderr),
            }
        } else {
            match serde_json::from_slice::<Outcome>(
                &fs::read(&outcome).context("worker produced no outcome")?,
            )? {
                Outcome::Compile(detail) => Failure {
                    kind: FailureKind::Compile,
                    input: None,
                    detail: detail.replace(path.to_string_lossy().as_ref(), "<source>"),
                },
                Outcome::Validate(detail) => Failure {
                    kind: FailureKind::Validate,
                    input: None,
                    detail: detail.replace(path.to_string_lossy().as_ref(), "<source>"),
                },
                Outcome::Execute(detail) => Failure {
                    kind: FailureKind::Execute,
                    input: None,
                    detail: detail.replace(path.to_string_lossy().as_ref(), "<source>"),
                },
                Outcome::Fuel(detail) => Failure {
                    kind: FailureKind::Fuel,
                    input: None,
                    detail: detail.replace(path.to_string_lossy().as_ref(), "<source>"),
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
                        input: Some(
                            actual
                                .iter()
                                .zip(expected)
                                .position(|(a, b)| match kind {
                                    FailureKind::Value => a.value != b.value,
                                    _ => a.trace != b.trace,
                                })
                                .unwrap_or(actual.len().min(expected.len())),
                        ),
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
    while attempts < limit {
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
    Ok((program, attempts))
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

fn execute(source: &str, path: &Path, asynchronous: bool) -> Outcome {
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
    match runtime.block_on(observations(&engine, &component, directory)) {
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

async fn observations(
    engine: &Engine,
    component: &Component,
    artifacts: &Path,
) -> Result<(Vec<Observation>, u64)> {
    let fuel = execution_fuel()?;
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
    save_environment(directory)?;
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
