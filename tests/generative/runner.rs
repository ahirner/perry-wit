//! Isolated execution and bounded reduction; a failed tool is never evidence of equivalence.
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use perry_wit::waffle_backend::{WaffleCompileOptions, compile_typescript_for_world};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wasmtime::component::{Component, Linker};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

use super::model::{Form, Generator, INPUTS, Number, Program, WIT, node_inputs};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub value: String,
    pub trace: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "stage", content = "detail")]
enum Outcome {
    Values(Vec<Observation>),
    Compile(String),
    Validate(String),
    Execute(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum FailureKind {
    Compile,
    Validate,
    Execute,
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
pub struct ProcessOutput {
    pub success: bool,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
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
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= limit {
            // The process may have exited between try_wait and kill.
            let _ = child.kill();
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(10));
    };
    Ok(ProcessOutput {
        success: status.success(),
        timed_out,
        stdout: fs::read_to_string(stdout)?,
        stderr: fs::read_to_string(stderr)?,
    })
}

pub fn campaign() -> Result<()> {
    let seed = setting("PERRY_GENERATIVE_SEED", 0_u64)?;
    let count = setting("PERRY_GENERATIVE_COUNT", 8_usize)?;
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
    let (mut programs, forms) = if let Some(path) = env::var_os("PERRY_GENERATIVE_REPLAY") {
        let program = serde_json::from_slice::<Program>(&fs::read(path)?)?;
        let forms = vec![program.form];
        (vec![program], forms)
    } else {
        (
            (0..count)
                .map(|index| Program {
                    expression: Generator::new(seed.wrapping_add(index as u64)).number(depth),
                    form: Form::Direct,
                })
                .collect::<Vec<_>>(),
            Form::ALL.to_vec(),
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
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(directory.join("inputs.json"), node_inputs())?;
    let mut report = json!({"seed":seed, "count":programs.len() * forms.len(), "depth":depth, "completed":0, "status":"running"});
    fs::write(
        directory.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    let mut tsc = Command::new("tsc");
    fs::write(
        directory.join("tsconfig.json"),
        r#"{"compilerOptions":{"noEmit":true,"strict":true,"target":"es2022","module":"esnext","skipLibCheck":true},"include":["case-*.ts"]}"#,
    )?;
    tsc.arg("-p").arg(directory.join("tsconfig.json"));
    let checked = command(&mut tsc, &directory, Duration::from_secs(60))?;
    ensure!(
        checked.success && !checked.timed_out,
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
            if let Some(failure) = compare(&path, &directory, &expected)? {
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
                    Ok(compare(&path, &directory, &expected)?
                        .is_some_and(|next| next.kind == failure.kind))
                })?;
                let minimal_path = directory.join("minimal.ts");
                fs::write(&minimal_path, minimal.source())?;
                fs::write(
                    directory.join("minimal.json"),
                    serde_json::to_vec_pretty(&minimal)?,
                )?;
                let expected = oracle(&minimal_path, &directory)?;
                let reproduced = compare(&minimal_path, &directory, &expected)?
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
    .arg(directory.join("inputs.json"));
    let output = command(&mut node, directory, Duration::from_secs(10))?;
    ensure!(
        output.success && !output.timed_out,
        "Node oracle failed for {}: {output:?}",
        path.display()
    );
    serde_json::from_str(&output.stdout).context("Node observations")
}

fn compare(path: &Path, directory: &Path, expected: &[Observation]) -> Result<Option<Failure>> {
    let outcome = directory.join("outcome.json");
    if outcome.exists() {
        fs::remove_file(&outcome)?;
    }
    let mut worker = Command::new(env::current_exe()?);
    worker
        .args(["--exact", "generative_worker", "--ignored", "--nocapture"])
        .env("PERRY_GENERATIVE_SOURCE", path)
        .env("PERRY_GENERATIVE_OUTCOME", &outcome);
    let output = command(&mut worker, directory, Duration::from_secs(30))?;
    let failure = if output.timed_out {
        Failure {
            kind: FailureKind::Timeout,
            detail: output.stderr,
        }
    } else if !output.success {
        Failure {
            kind: FailureKind::Crash,
            detail: format!("{}\n{}", output.stdout, output.stderr),
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
            Outcome::Values(actual) => {
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
    let outcome = execute(&source, &path);
    fs::write(destination, serde_json::to_vec(&outcome)?)?;
    Ok(())
}

fn execute(source: &str, path: &Path) -> Outcome {
    let mut resolve = wit_parser::Resolve::default();
    let package = resolve.push_str("generated.wit", WIT).unwrap();
    let world = resolve.select_world(&[package], Some("generated")).unwrap();
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
    config.consume_fuel(true);
    let engine = Engine::new(&config).unwrap();
    let component = match Component::new(&engine, compiled.component.unwrap()) {
        Ok(component) => component,
        Err(error) => return Outcome::Validate(format!("{error:#}")),
    };
    match observations(&engine, &component) {
        Ok(values) => Outcome::Values(values),
        Err(error) => Outcome::Execute(format!("{error:#}")),
    }
}

#[derive(wasmtime::component::ComponentType, wasmtime::component::Lift)]
#[component(record)]
struct GuestObservation {
    value: f64,
    trace: Vec<f64>,
}

fn observations(engine: &Engine, component: &Component) -> Result<Vec<Observation>> {
    let mut store = Store::new(
        engine,
        StoreLimitsBuilder::new()
            .memory_size(8 * 1024 * 1024)
            .build(),
    );
    store.limiter(|limits: &mut StoreLimits| limits);
    store.set_fuel(1_000_000)?;
    let instance = Linker::new(engine).instantiate(&mut store, component)?;
    let run = instance.get_typed_func::<(f64,), (GuestObservation,)>(&mut store, "run")?;
    let mut values = Vec::new();
    // Reuse the instance to catch retained state and allocation lifetime errors.
    for input in INPUTS {
        store.set_fuel(1_000_000)?;
        let (value,) = run
            .call(&mut store, (input,))
            .map_err(|error| anyhow::anyhow!("input {input:?}: {error:#}"))?;
        values.push(Observation {
            value: number(value.value),
            trace: value.trace.into_iter().map(number).collect(),
        });
    }
    Ok(values)
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
    let directory = tempfile::tempdir()?;
    let directory = directory.path();
    fs::write(directory.join("oracle.mjs"), include_str!("oracle.mjs"))?;
    fs::write(directory.join("inputs.json"), node_inputs())?;
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
            compare(&path, directory, &expected)?.is_none(),
            "unmodified source must pass"
        );
        let path = directory.join("fault.ts");
        fs::write(&path, source.replace("mark(trace, ", "identity("))?;
        Ok(compare(&path, directory, &expected)?
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
