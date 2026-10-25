//! Isolated effectful adapters for Node oracle and Wasmtime component execution.
//!
//! [Executor] runs the Node oracle and executes the compiled Wasm component under wasmtime.
use crate::{
    program::{ENV_FIXTURE, FIXTURE, INPUTS, WIT},
    registry::Case,
};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    thread,
    time::{Duration, Instant},
};

mod oracle;
mod worker;
use oracle::{encoded_inputs, is_typechecked, link_handler_types, run_node_oracle, run_tsc_batch};

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
#[derive(Clone, Serialize)]
#[serde(tag = "stage", content = "detail", rename_all = "snake_case")]
pub(crate) enum Outcome {
    Values {
        observations: Vec<Observation>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        max_fuel_used: u64,
        component_bytes: usize,
        /// Fuel consumed by each call in input order, populated for component executions.
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
#[derive(Debug, Serialize)]
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
pub(crate) fn check_metamorphic(evidence: &[Evidence]) -> Result<()> {
    fn same_observations(left: &Outcome, right: &Outcome) -> bool {
        match (left, right) {
            (
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
            ) => a == b && a_out == b_out && a_err == b_err,
            (Outcome::Command(a), Outcome::Command(b)) => a == b,
            (Outcome::Compile(_), Outcome::Compile(_)) => true,
            _ => false,
        }
    }
    if let Some((first, rest)) = evidence.split_first() {
        for result in rest {
            ensure!(
                same_observations(&first.oracle, &result.oracle),
                "metamorphic form changed Node observations: {:?} != {:?}",
                first.oracle,
                result.oracle
            );
            ensure!(
                same_observations(&first.component, &result.component),
                "metamorphic form changed component observations: {:?} != {:?}",
                first.component,
                result.component
            );
        }
    }
    Ok(())
}

pub fn check_equivalence(case: &Case, evidence: &Evidence) -> Result<()> {
    equivalence(case, &evidence.oracle, &evidence.component, evidence)
}

/// Compare an oracle outcome against a component outcome, reporting `evidence` on mismatch.
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

/// Capture child output through temporary files and reap timed-out processes.
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
    pub fn new(limits: Limits) -> Self {
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
        self.check_variants(&case.variants(), directory)
    }

    fn check_variants(
        &self,
        variants: &[std::borrow::Cow<'_, Case>],
        directory: &Path,
    ) -> Result<Vec<Evidence>> {
        let evidence = thread::scope(|scope| -> Result<Vec<Evidence>> {
            let handles = variants
                .iter()
                .enumerate()
                .map(|(index, variant)| {
                    let directory = directory.join(index.to_string());
                    spawn_worker(scope, move || {
                        self.execute_source(variant, &variant.source(), &directory)
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("variant thread panicked"))?
                })
                .collect()
        })?;
        check_metamorphic(&evidence)?;
        Ok(evidence)
    }

    pub(crate) fn execute_source(
        &self,
        case: &Case,
        source: &str,
        directory: &Path,
    ) -> Result<Evidence> {
        self.prepare_directory(case, source, directory)?;
        let (oracle, (component, control)) = thread::scope(|scope| -> Result<_> {
            let oracle_handle = spawn_worker(scope, || self.run_oracle(case, directory))?;
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

    fn prepare_directory(&self, case: &Case, source: &str, directory: &Path) -> Result<()> {
        fs::create_dir_all(directory)?;
        let path = directory.join("case.ts");
        fs::write(&path, source)?;
        if matches!(case, Case::Handler(_)) {
            link_handler_types(directory)?;
        }
        crate::scaling::override_inputs(case, directory)?;
        if !matches!(case, Case::Reject { .. }) {
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

    fn run_oracle(&self, case: &Case, directory: &Path) -> Result<Outcome> {
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
                    .arg("--no-warnings")
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
            run_node_oracle(
                &path,
                inputs,
                case,
                Duration::from_secs(self.limits.timeout_seconds),
            )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn transformed_commands_use_their_own_node_observations() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let executor = Executor::new(Limits::default());
        let variants = [0, 1].map(|value| {
            Cow::Owned(Case::Command {
                source: format!("console.log({value});"),
                exit: crate::registry::ExitStatus::Success,
            })
        });
        let error = executor
            .check_variants(&variants, directory.path())
            .unwrap_err();
        ensure!(
            error
                .to_string()
                .contains("metamorphic form changed Node observations"),
            "wrong failure: {error:#}"
        );
        Ok(())
    }
}
