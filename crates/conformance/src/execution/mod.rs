//! Isolated effectful adapters. Models and observation checks do not perform I/O.
//!
//! The parent [Executor] runs the Node oracle and spawns a `worker` process per case; the worker
//! ([worker]) compiles the case and runs the component under wasmtime.
use crate::{
    program::{ENV_FIXTURE, FIXTURE, INPUTS, WIT},
    registry::Case,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

mod worker;
pub use worker::worker;

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
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
pub(crate) struct Observation {
    pub(crate) value: String,
    pub(crate) trace: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "stage", content = "detail", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Values {
        observations: Vec<Observation>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        max_fuel_used: u64,
        component_bytes: usize,
        /// Fuel consumed by each call, in input order; empty for the Node oracle.
        #[serde(default)]
        fuel_used: Vec<u64>,
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
                fuel_used,
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
                .field("fuel_used", fuel_used)
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
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub timed_out: bool,
}
impl Captured {
    pub fn success(&self) -> bool {
        !self.timed_out && self.exit == Some(0)
    }
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub(crate) oracle: Outcome,
    pub(crate) component: Outcome,
    /// The unchanged program's component outcome, for fault cases only.
    pub(crate) control: Option<Outcome>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Fault {
    Value,
    Trace,
    Resource,
}
impl Fault {
    fn source(self, source: String) -> String {
        match self {
            Self::Value => source.replace("value:value", "value:(value === 0 ? 1 : 0)"),
            Self::Trace => source.replace("mark(trace, ", "identity("),
            Self::Resource => source,
        }
    }
}
pub fn check_fault(case: &Case, evidence: &Evidence) -> Result<()> {
    let Case::Fault { fault, .. } = case else {
        bail!("fault checker requires a fault case")
    };
    let control = evidence
        .control
        .as_ref()
        .context("fault case lacks unchanged component evidence")?;
    // A fault case checks like its unfaulted program: the oracle must match the control.
    equivalence(case, &evidence.oracle, control, evidence)?;
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
        _ => bail!("injected fault produced the wrong failure: {evidence:?}"),
    }
    Ok(())
}
/// The worker's input; borrowed when written, owned when read back.
#[derive(Serialize, Deserialize)]
struct Request<'a> {
    case: Cow<'a, Case>,
    limits: Limits,
}

pub(crate) fn check_metamorphic(evidence: &[Evidence]) -> Result<()> {
    if let Some((first, rest)) = evidence.split_first()
        && let Outcome::Values {
            observations: baseline,
            stdout,
            stderr,
            ..
        } = &first.oracle
    {
        for result in rest {
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

pub(crate) fn check_equivalence(case: &Case, evidence: &Evidence) -> Result<()> {
    equivalence(case, &evidence.oracle, &evidence.component, evidence)
}

/// Compares an oracle outcome with a component outcome; `evidence` is reported on failure.
fn equivalence(
    case: &Case,
    oracle: &Outcome,
    component: &Outcome,
    evidence: &Evidence,
) -> Result<()> {
    match (case, oracle, component) {
        (Case::Reject { diagnostic, .. }, _, Outcome::Compile(message)) => ensure!(
            !diagnostic.is_empty() && message.contains(diagnostic),
            "unexpected rejection: {message}"
        ),
        (Case::Reject { .. }, _, _) => bail!("expected compiler diagnostic: {evidence:?}"),
        (
            _,
            Outcome::Values {
                observations: a,
                stdout: a_out,
                stderr: a_err,
                ..
            },
            Outcome::Values {
                observations: b,
                stdout: b_out,
                stderr: b_err,
                ..
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
            ensure!(
                a_out == b_out && a_err == b_err,
                "function output bytes differ"
            );
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
        _ => bail!("execution did not establish equivalence: {evidence:?}"),
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
        thread::sleep(Duration::from_millis(1));
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
    limits: Limits,
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
    fn execute(&self, case: &Case, directory: &Path) -> Result<Evidence> {
        self.execute_source(case, &case.source(), directory)
    }
    pub(crate) fn execute_source(
        &self,
        case: &Case,
        source: &str,
        directory: &Path,
    ) -> Result<Evidence> {
        fs::create_dir_all(directory)?;
        let path = directory.join("case.ts");
        fs::write(&path, source)?;
        environment(directory)?;
        crate::scaling::override_inputs(case, directory)?;
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
                    .env("PERRY_CONFORMANCE", ENV_FIXTURE),
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
                    fuel_used: Vec::new(),
                }
            }
        };
        let request = directory.join("request.json");
        let destination = directory.join("outcome.json");
        fs::write(
            &request,
            serde_json::to_vec(&Request {
                case: Cow::Borrowed(case),
                limits: self.limits,
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

fn cached_handler_sdk() -> Result<&'static [(PathBuf, Vec<u8>)]> {
    static SDK_FILES: std::sync::OnceLock<Vec<(PathBuf, Vec<u8>)>> = std::sync::OnceLock::new();
    if let Some(files) = SDK_FILES.get() {
        return Ok(files.as_slice());
    }
    let temp = tempfile::tempdir()?;
    let out_dir = temp.path().join("handler-types");
    perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/handler"),
        world: Some("handler".into()),
        out_dir: out_dir.clone(),
        project_root: Some(temp.path().into()),
        entry: "case.ts".into(),
        initialize_tsconfig: false,
    })?;
    let mut files = Vec::new();
    for entry in fs::read_dir(&out_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            files.push((PathBuf::from(entry.file_name()), fs::read(&path)?));
        }
    }
    let _ = SDK_FILES.set(files);
    Ok(SDK_FILES.get().expect("initialized").as_slice())
}

fn environment(directory: &Path) -> Result<()> {
    static INPUTS_JSON: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();
    static ASYNC_WIT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    fs::write(directory.join("fixture.txt"), FIXTURE)?;
    fs::write(directory.join("world.wit"), WIT)?;
    fs::write(
        directory.join("async-world.wit"),
        ASYNC_WIT.get_or_init(async_wit),
    )?;
    fs::write(
        directory.join("capabilities.d.ts"),
        "/// <reference path=\"./handler-types/runtime.d.ts\" />\ndeclare module \"test:generated/control\" { export function release():void; }",
    )?;
    let handler_types = directory.join("handler-types");
    fs::create_dir_all(&handler_types)?;
    for (rel, bytes) in cached_handler_sdk()? {
        let target = handler_types.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(target, bytes)?;
    }
    fs::write(directory.join("oracle.mjs"), include_str!("../oracle.mjs"))?;
    let inputs = INPUTS_JSON
        .get_or_init(|| serde_json::to_vec(&INPUTS.map(number)).expect("inputs serialize"));
    fs::write(directory.join("inputs.json"), inputs)?;
    Ok(())
}

fn async_wit() -> String {
    WIT.replace(
        "export run: func",
        "include wasi:cli/imports@0.3.0; export run: async func",
    )
}

pub fn number(value: f64) -> String {
    if value.is_nan() {
        "NaN".into()
    } else {
        format!("{:016x}", value.to_bits())
    }
}
