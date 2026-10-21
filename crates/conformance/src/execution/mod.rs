//! Isolated effectful adapters. Models and observation checks do not perform I/O.
//!
//! [Executor] runs the Node oracle and executes the compiled Wasm component under wasmtime.
use crate::{
    program::{ENV_FIXTURE, FIXTURE, INPUTS, WIT},
    registry::Case,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

mod worker;
pub use worker::worker;

const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

fn spawn_worker<'scope, 'env, F, T>(
    scope: &'scope thread::Scope<'scope, 'env>,
    f: F,
) -> Result<thread::ScopedJoinHandle<'scope, T>>
where
    F: FnOnce() -> T + Send + 'scope,
    T: Send + 'scope,
{
    Ok(thread::Builder::new()
        .stack_size(WORKER_STACK_BYTES)
        .spawn_scoped(scope, f)?)
}

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
/// The fact-analysis precision a generated case compiles under. The oracle ignores it, so each
/// setting is checked against the same expected observations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactLimits {
    #[default]
    Default,
    /// One origin and one precise context: every aggregate value is summarized.
    Summarized,
    /// Two origins and two precise contexts: summarization sets in after a few origins.
    Narrow,
}

impl FactLimits {
    /// Run `compile` under these limits.
    fn apply<R>(self, compile: impl FnOnce() -> R) -> R {
        match self {
            Self::Default => compile(),
            Self::Summarized => perry_mir::with_fact_limits(1, 1, compile),
            Self::Narrow => perry_mir::with_fact_limits(2, 2, compile),
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
impl Evidence {
    pub fn first_values(&self) -> Result<(&str, &str)> {
        let (
            Outcome::Values {
                observations: oracle,
                ..
            },
            Outcome::Values {
                observations: component,
                ..
            },
        ) = (&self.oracle, &self.component)
        else {
            bail!("expected both oracle and component to succeed with values, got {self:?}");
        };
        let (Some(o), Some(c)) = (oracle.first(), component.first()) else {
            bail!("expected non-empty observations, got {self:?}");
        };
        Ok((&o.value, &c.value))
    }
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
    if let Some((first, rest)) = evidence.split_first()
        && let Outcome::Values {
            observations: baseline,
            stdout,
            stderr,
            ..
        } = &first.component
    {
        for result in rest {
            if let Outcome::Values {
                observations,
                stdout: output,
                stderr: errors,
                ..
            } = &result.component
            {
                ensure!(
                    observations == baseline && output == stdout && errors == stderr,
                    "metamorphic form changed component observations"
                );
            }
        }
    }
    Ok(())
}

pub fn check_equivalence(case: &Case, evidence: &Evidence) -> Result<()> {
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
                a.len() == case.inputs().len(),
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
    let mut polls = 0u32;
    let (status, timed_out) = loop {
        if let Some(status) = child.try_wait()? {
            break (status, false);
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            break (child.wait()?, true);
        }
        polls = polls.saturating_add(1);
        let pause = if polls <= 25 {
            Duration::from_micros(200)
        } else {
            Duration::from_millis(1)
        };
        thread::sleep(pause);
    };
    Ok(Captured {
        exit: status.code(),
        timed_out,
        stdout: fs::read(logs.join("stdout"))?,
        stderr: fs::read(logs.join("stderr"))?,
    })
}

pub struct Executor {
    limits: Limits,
    batch_tsc: Mutex<Option<Vec<(PathBuf, String)>>>,
}
impl Executor {
    pub fn new(_executable: PathBuf, limits: Limits) -> Self {
        Self {
            limits,
            batch_tsc: Mutex::new(None),
        }
    }

    pub(crate) fn begin_typecheck_batch(&self) {
        *self.batch_tsc.lock().expect("tsc batch lock") = Some(Vec::new());
    }

    pub(crate) fn flush_typecheck_batch(&self, directory: &Path) -> Result<()> {
        let pending = self
            .batch_tsc
            .lock()
            .expect("tsc batch lock")
            .take()
            .unwrap_or_default();
        if pending.is_empty() {
            return Ok(());
        }
        run_tsc_batch(&pending, directory)
    }

    pub fn check(&self, case: &Case, directory: &Path) -> Result<Vec<Evidence>> {
        let variants = case.variants();
        let Some((first_variant, rest_variants)) = variants.split_first() else {
            return Ok(Vec::new());
        };
        if rest_variants.is_empty() {
            let evidence = self.execute(first_variant, &directory.join("0"))?;
            return Ok(vec![evidence]);
        }
        // Variant 0 establishes the oracle observation and schedules TypeScript verification;
        // metamorphic variants 1..N share Variant 0's oracle outcome and compile/run in parallel.
        let first_dir = directory.join("0");
        let first_source = first_variant.source();
        self.prepare_directory(first_variant, &first_source, &first_dir, true)?;
        let rest_prepared = rest_variants
            .iter()
            .enumerate()
            .map(|(i, variant)| {
                let dir = directory.join((i + 1).to_string());
                let src = variant.source();
                self.prepare_directory(variant, &src, &dir, false)?;
                Ok((variant, src, dir))
            })
            .collect::<Result<Vec<_>>>()?;

        let (first_oracle, first_component, rest_components) = thread::scope(|scope| -> Result<_> {
            let oracle_handle =
                spawn_worker(scope, || self.run_oracle(first_variant, &first_source, &first_dir))?;
            let first_comp_handle = spawn_worker(scope, || {
                self.run_component(first_variant, &first_source, &first_dir)
            })?;
            let rest_handles = rest_prepared
                .iter()
                .map(|(variant, src, dir)| {
                    spawn_worker(scope, move || self.run_component(variant, src, dir))
                })
                .collect::<Result<Vec<_>>>()?;

            let oracle = oracle_handle
                .join()
                .map_err(|_| anyhow::anyhow!("oracle thread panicked"))??;
            let (first_comp, first_ctrl) = first_comp_handle
                .join()
                .map_err(|_| anyhow::anyhow!("component thread panicked"))??;
            let mut rest_results = Vec::with_capacity(rest_handles.len());
            for handle in rest_handles {
                rest_results.push(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("variant thread panicked"))??,
                );
            }
            Ok((oracle, (first_comp, first_ctrl), rest_results))
        })?;

        let mut evidence = Vec::with_capacity(variants.len());
        let first_ev = Evidence {
            oracle: first_oracle.clone(),
            component: first_component.0,
            control: first_component.1,
        };
        fs::write(
            first_dir.join("evidence.json"),
            serde_json::to_vec(&first_ev)?,
        )?;
        evidence.push(first_ev);

        for ((_, _, dir), (component, control)) in rest_prepared.into_iter().zip(rest_components) {
            let ev = Evidence {
                oracle: first_oracle.clone(),
                component,
                control,
            };
            fs::write(dir.join("evidence.json"), serde_json::to_vec(&ev)?)?;
            evidence.push(ev);
        }

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
        self.prepare_directory(case, source, directory, true)?;
        let (oracle, (component, control)) = thread::scope(|scope| -> Result<_> {
            let oracle_handle = spawn_worker(scope, || self.run_oracle(case, source, directory))?;
            let comp_handle = spawn_worker(scope, || self.run_component(case, source, directory))?;
            let oracle = oracle_handle
                .join()
                .map_err(|_| anyhow::anyhow!("oracle thread panicked"))??;
            let comp = comp_handle
                .join()
                .map_err(|_| anyhow::anyhow!("component thread panicked"))??;
            Ok((oracle, comp))
        })?;
        let evidence = Evidence {
            oracle,
            component,
            control,
        };
        fs::write(
            directory.join("evidence.json"),
            serde_json::to_vec(&evidence)?,
        )?;
        Ok(evidence)
    }

    fn prepare_directory(
        &self,
        case: &Case,
        source: &str,
        directory: &Path,
        check_ts: bool,
    ) -> Result<()> {
        fs::create_dir_all(directory)?;
        let path = directory.join("case.ts");
        fs::write(&path, source)?;
        if matches!(case, Case::Handler(_)) {
            link_handler_types(directory)?;
        }
        crate::scaling::override_inputs(case, directory)?;
        if check_ts && !matches!(case, Case::Reject { .. }) {
            self.schedule_typecheck(case, &path, source, directory)?;
        }
        Ok(())
    }

    fn schedule_typecheck(
        &self,
        case: &Case,
        path: &Path,
        source: &str,
        directory: &Path,
    ) -> Result<()> {
        if is_typechecked(source) {
            return Ok(());
        }
        // Non-module Command scripts share global scope if batched, so check them individually.
        if !matches!(case, Case::Command { .. })
            && let Some(batch) = &mut *self.batch_tsc.lock().expect("tsc batch lock")
        {
            if !batch.iter().any(|(_, s)| s == source) {
                if !matches!(case, Case::Handler(_)) {
                    link_handler_types(directory)?;
                }
                batch.push((path.to_path_buf(), source.to_owned()));
            }
            return Ok(());
        }
        if !matches!(case, Case::Handler(_)) {
            link_handler_types(directory)?;
        }
        run_tsc_batch(&[(path.to_path_buf(), source.to_owned())], directory)
    }

    fn run_oracle(&self, case: &Case, _source: &str, directory: &Path) -> Result<Outcome> {
        let path = directory.join("case.ts");
        if matches!(case, Case::Reject { .. }) {
            Ok(Outcome::Compile("expected source-domain rejection".into()))
        } else if let Case::Aggregate(agg) = case
            && agg.uses_wide_exact_int()
        {
            Ok(agg.expected_outcome())
        } else if matches!(case, Case::Command { .. }) {
            Ok(Outcome::Command(command(
                Command::new("node")
                    .args([
                        "--no-warnings",
                        "--harmony-temporal",
                        "--experimental-transform-types",
                    ])
                    .arg(&path)
                    .env("PERRY_CONFORMANCE", ENV_FIXTURE)
                    .env("TZ", "UTC"),
                directory,
                Duration::from_secs(self.limits.timeout_seconds),
            )?))
        } else {
            let inputs_path = directory.join("inputs.json");
            let custom_inputs: Option<Vec<String>> = if inputs_path.exists() {
                Some(serde_json::from_slice(&fs::read(inputs_path)?)?)
            } else {
                None
            };
            let inputs = custom_inputs
                .as_deref()
                .unwrap_or_else(|| &encoded_inputs()[..case.inputs().len()]);
            run_node_oracle(&path, inputs, case)
        }
    }

    fn run_component(
        &self,
        case: &Case,
        source: &str,
        directory: &Path,
    ) -> Result<(Outcome, Option<Outcome>)> {
        let path = directory.join("case.ts");
        let component = worker::execute_in_process(case, source, directory, &path, &self.limits)?;
        let control = if let Case::Fault { program, .. } = case {
            let control_dir = directory.join("control");
            fs::create_dir_all(&control_dir)?;
            let control_path = control_dir.join("case.ts");
            fs::write(&control_path, source)?;
            Some(worker::execute_in_process(
                &Case::Program(program.clone()),
                source,
                &control_dir,
                &control_path,
                &self.limits,
            )?)
        } else {
            None
        };
        Ok((component, control))
    }
}

struct SharedSdk {
    _temp: tempfile::TempDir,
    handler_types: PathBuf,
    capabilities_dts: PathBuf,
    oracle_mjs: PathBuf,
}

fn shared_sdk() -> Result<&'static SharedSdk> {
    static SDK: OnceLock<SharedSdk> = OnceLock::new();
    if let Some(sdk) = SDK.get() {
        return Ok(sdk);
    }
    let temp = tempfile::tempdir()?;
    let handler_types = temp.path().join("handler-types");
    perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/handler"),
        world: Some("handler".into()),
        out_dir: handler_types.clone(),
        project_root: Some(temp.path().into()),
        entry: "case.ts".into(),
        initialize_tsconfig: false,
    })?;
    let capabilities_dts = temp.path().join("capabilities.d.ts");
    fs::write(
        &capabilities_dts,
        "/// <reference path=\"./handler-types/runtime.d.ts\" />\ndeclare module \"test:generated/control\" { export function release():void; }",
    )?;
    let oracle_mjs = temp.path().join("oracle.mjs");
    fs::write(&oracle_mjs, include_str!("../oracle.mjs"))?;
    let _ = SDK.set(SharedSdk {
        _temp: temp,
        handler_types,
        capabilities_dts,
        oracle_mjs,
    });
    Ok(SDK.get().expect("sdk initialized"))
}

fn link_handler_types(directory: &Path) -> Result<()> {
    let target = directory.join("handler-types");
    if target.exists() || target.symlink_metadata().is_ok() {
        return Ok(());
    }
    let sdk = shared_sdk()?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&sdk.handler_types, &target)?;
    #[cfg(not(unix))]
    {
        fs::create_dir_all(&target)?;
        for entry in fs::read_dir(&sdk.handler_types)? {
            let entry = entry?;
            if entry.path().is_file() {
                fs::copy(entry.path(), target.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
}

fn typechecked_cache() -> &'static Mutex<HashSet<String>> {
    static CACHE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_typechecked(source: &str) -> bool {
    typechecked_cache()
        .lock()
        .expect("typecheck cache lock")
        .contains(source)
}

fn run_tsc_batch(files: &[(PathBuf, String)], directory: &Path) -> Result<()> {
    let sdk = shared_sdk()?;
    let mut cmd = Command::new("tsc");
    cmd.current_dir(directory).args([
        "--ignoreConfig",
        "--noEmit",
        "--strict",
        "--target",
        "esnext",
        "--module",
        "esnext",
        "--skipLibCheck",
    ]);
    for (path, _) in files {
        cmd.arg(path);
    }
    cmd.arg(&sdk.capabilities_dts);
    let checked = command(&mut cmd, directory, Duration::from_secs(60))?;
    ensure!(
        checked.success(),
        "source failed TypeScript: {}",
        String::from_utf8_lossy(&checked.stdout)
    );
    let mut cache = typechecked_cache().lock().expect("typecheck cache lock");
    for (_, source) in files {
        cache.insert(source.clone());
    }
    Ok(())
}

struct NodeDaemon {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

#[derive(Serialize)]
struct OracleCaseMeta<'a> {
    kind: &'static str,
    #[serde(rename = "payloadPath", skip_serializing_if = "Option::is_none")]
    payload_path: Option<&'a Path>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    status: u16,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

#[derive(Serialize)]
struct OracleRequest<'a> {
    #[serde(rename = "casePath")]
    case_path: &'a Path,
    #[serde(rename = "stdoutPath")]
    stdout_path: &'a Path,
    #[serde(rename = "stderrPath")]
    stderr_path: &'a Path,
    inputs: &'a [String],
    fixture: &'static str,
    #[serde(rename = "testCase")]
    test_case: OracleCaseMeta<'a>,
}

#[derive(Deserialize)]
struct OracleResponse {
    ok: bool,
    #[serde(default)]
    observations: Vec<Observation>,
    #[serde(rename = "hasStdout", default)]
    has_stdout: bool,
    #[serde(rename = "hasStderr", default)]
    has_stderr: bool,
    #[serde(default)]
    error: String,
}

impl NodeDaemon {
    fn spawn() -> Result<Self> {
        let sdk = shared_sdk()?;
        let mut child = Command::new("node")
            .args([
                "--disable-warning=ExperimentalWarning",
                "--harmony-temporal",
                "--experimental-strip-types",
            ])
            .arg(&sdk.oracle_mjs)
            .env("TZ", "UTC")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().context("node daemon stdin")?;
        let stdout = BufReader::new(child.stdout.take().context("node daemon stdout")?);
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    fn call(&mut self, req: &OracleRequest<'_>) -> Result<Outcome> {
        serde_json::to_writer(&mut self.stdin, req)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut line = String::new();
        let read = self.stdout.read_line(&mut line)?;
        ensure!(read > 0, "node daemon closed stdout unexpectedly");
        let resp: OracleResponse = serde_json::from_str(&line).context("Node daemon response")?;
        if resp.ok {
            let stdout = if resp.has_stdout {
                fs::read(req.stdout_path)?
            } else {
                Vec::new()
            };
            let stderr = if resp.has_stderr {
                fs::read(req.stderr_path)?
            } else {
                Vec::new()
            };
            Ok(Outcome::Values {
                observations: resp.observations,
                stdout,
                stderr,
                max_fuel_used: 0,
                component_bytes: 0,
                fuel_used: Vec::new(),
            })
        } else {
            Ok(Outcome::Execute(resp.error))
        }
    }
}

impl Drop for NodeDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_node_oracle(case_path: &Path, inputs: &[String], test_case: &Case) -> Result<Outcome> {
    static DAEMON: OnceLock<Mutex<Option<NodeDaemon>>> = OnceLock::new();
    let dir = case_path.parent().context("case directory")?;
    let payload_path = dir.join("payload.bin");
    let stdout_path = dir.join("oracle-stdout.bin");
    let stderr_path = dir.join("oracle-stderr.bin");
    let meta = match test_case {
        Case::Stream(s) => {
            fs::write(&payload_path, &s.bytes)?;
            OracleCaseMeta {
                kind: "stream",
                payload_path: Some(&payload_path),
                truncated: false,
                status: 0,
            }
        }
        Case::Fetch(f) => {
            fs::write(&payload_path, &f.bytes)?;
            OracleCaseMeta {
                kind: "fetch",
                payload_path: Some(&payload_path),
                truncated: f.truncated,
                status: f.status,
            }
        }
        Case::Handler(h) => {
            fs::write(&payload_path, &h.bytes)?;
            OracleCaseMeta {
                kind: "handler",
                payload_path: Some(&payload_path),
                truncated: false,
                status: 0,
            }
        }
        Case::Lifecycle(_) => OracleCaseMeta {
            kind: "lifecycle",
            payload_path: None,
            truncated: false,
            status: 0,
        },
        _ => OracleCaseMeta {
            kind: "program",
            payload_path: None,
            truncated: false,
            status: 0,
        },
    };
    let req = OracleRequest {
        case_path,
        stdout_path: &stdout_path,
        stderr_path: &stderr_path,
        inputs,
        fixture: FIXTURE,
        test_case: meta,
    };
    let mutex = DAEMON.get_or_init(|| Mutex::new(None));
    let mut guard = mutex.lock().expect("node daemon lock");
    if guard.is_none() {
        *guard = Some(NodeDaemon::spawn()?);
    }
    match guard.as_mut().expect("daemon initialized").call(&req) {
        Ok(outcome) => Ok(outcome),
        Err(_) => {
            *guard = Some(NodeDaemon::spawn()?);
            guard.as_mut().expect("daemon respawned").call(&req)
        }
    }
}

fn encoded_inputs() -> &'static [String] {
    static ENCODED: OnceLock<Vec<String>> = OnceLock::new();
    ENCODED
        .get_or_init(|| INPUTS.map(number).to_vec())
        .as_slice()
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
