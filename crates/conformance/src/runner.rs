//! Campaign orchestration and structured conformance evidence.
use crate::{
    execution::{Executor, Limits, check_metamorphic},
    registry::{self, Case, Contract, ExpectedOutcome},
};
use anyhow::{Context, Result, ensure};
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestError, TestRunner};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Settings {
    pub select: String,
    pub cases: u32,
    pub depth: u32,
    pub seed: u64,
    pub shrink_limit: u32,
    pub limits: Limits,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            select: String::new(),
            cases: 8,
            depth: 3,
            seed: 0,
            shrink_limit: 100,
            limits: Limits::default(),
        }
    }
}
impl Settings {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            (1..=10000).contains(&self.cases),
            "cases must be in 1..=10000"
        );
        ensure!(self.depth <= 6, "depth must be at most 6");
        ensure!(
            (1..=100_000_000).contains(&self.limits.fuel),
            "fuel must be in 1..=100000000"
        );
        ensure!(
            self.limits.memory_bytes > 0 && self.limits.timeout_seconds > 0,
            "memory and timeout must be positive"
        );
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    NotRun,
    Running,
    Passed,
    Failed,
    /// Unlisted failing contract tracked as pending work in the campaign summary.
    Pending,
    /// Listed in `deferred.txt` as outside the accepted domain.
    Deferred,
}
#[derive(Serialize)]
struct PartitionResult {
    id: &'static str,
    status: Status,
}
#[derive(Serialize)]
struct ContractResult {
    id: &'static str,
    description: &'static str,
    specification: &'static str,
    domain: &'static str,
    status: Status,
    partitions: Vec<PartitionResult>,
    expected_outcomes: &'static [ExpectedOutcome],
    generated_cases: u32,
    executions: u32,
    error: Option<String>,
}
#[derive(Serialize)]
struct Report<'a> {
    schema: u32,
    settings: &'a Settings,
    source_sha256: String,
    revision: String,
    versions: serde_json::Value,
    contracts: Vec<ContractResult>,
    source_unchanged: bool,
}
#[derive(Serialize, Deserialize)]
pub struct ConcreteCase {
    pub case: Case,
    pub source: String,
}
#[derive(Serialize, Deserialize)]
pub struct SavedCase {
    pub contract: String,
    pub case: Case,
    pub source: String,
    pub metamorphic: Vec<ConcreteCase>,
    pub settings: Settings,
    pub source_sha256: String,
    pub revision: String,
    pub versions: serde_json::Value,
}

/// The workspace root used to identify the checked sources.
fn root() -> Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .context("workspace root")
}

fn evidence_directory() -> Result<PathBuf> {
    let target = match env::var_os("CARGO_TARGET_DIR").filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => root()?.join("target"),
    };
    Ok(target.join("conformance"))
}
fn identity() -> Result<(String, String)> {
    let root = root()?;
    let output = Command::new("git")
        .current_dir(&root)
        .args([
            "ls-files",
            "--cached",
            "--others",
            "--exclude-standard",
            "-z",
        ])
        .output()?;
    ensure!(output.status.success(), "cannot enumerate source files");
    let paths = String::from_utf8(output.stdout)?;
    let mut paths = paths
        .split('\0')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>();
    paths.sort_unstable();
    let mut digest = Sha256::new();
    for path in paths {
        digest.update(path);
        digest.update([0]);
        match fs::read(root.join(path)) {
            Ok(bytes) => digest.update(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => digest.update(b"missing"),
            Err(error) => return Err(error.into()),
        }
    }
    let revision = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", "HEAD"])
        .output()?;
    ensure!(revision.status.success(), "cannot read source revision");
    Ok((
        format!("{:x}", digest.finalize()),
        String::from_utf8(revision.stdout)?.trim().into(),
    ))
}
fn version(program: &str) -> Result<String> {
    let output = Command::new(program).arg("--version").output()?;
    ensure!(output.status.success(), "cannot identify {program}");
    Ok(String::from_utf8(output.stdout)?.trim().into())
}
fn versions() -> Result<serde_json::Value> {
    static VERSIONS: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
    if let Some(cached) = VERSIONS.get() {
        return Ok(cached.clone());
    }
    let value = serde_json::json!({"node":version("node")?,"typescript":version("tsc")?,"wasmtime":version("wasmtime")?,"rust":version("rustc")?});
    let _ = VERSIONS.set(value.clone());
    Ok(value)
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = tempfile::NamedTempFile::new_in(path.parent().context("report directory")?)?;
    serde_json::to_writer_pretty(temporary.as_file(), value)?;
    temporary.persist(path)?;
    Ok(())
}
fn evaluate(contract: &Contract, case: &Case, executor: &Executor, directory: &Path) -> Result<()> {
    ensure!(
        contract
            .catalog()?
            .expected_outcomes
            .contains(&case.expected_outcome()),
        "generated input changes the contract's declared expected outcome"
    );
    let (variants, evidence): (Vec<_>, Vec<_>) =
        executor.check(case, directory)?.into_iter().unzip();
    for (variant, evidence) in variants.iter().zip(&evidence) {
        (contract.check)(variant, evidence)?;
    }
    check_metamorphic(&evidence)
}
fn save_case(path: &Path, contract: &str, case: Case, settings: &Settings) -> Result<()> {
    let source = case.source();
    fs::write(path.with_extension("ts"), &source)?;
    // The first variant is the case itself.
    let metamorphic = case
        .variants()
        .into_iter()
        .skip(1)
        .map(|case| {
            let case = case.into_owned();
            ConcreteCase {
                source: case.source(),
                case,
            }
        })
        .collect::<Vec<_>>();
    for (index, variant) in metamorphic.iter().enumerate() {
        fs::write(
            path.with_extension(format!("form-{index}.ts")),
            &variant.source,
        )?;
    }
    let (source_sha256, revision) = identity()?;
    write_json(
        path,
        &SavedCase {
            contract: contract.into(),
            source,
            metamorphic,
            case,
            settings: settings.clone(),
            source_sha256,
            revision,
            versions: versions()?,
        },
    )
}

pub fn campaign(settings: Settings) -> Result<PathBuf> {
    settings.validate()?;
    let contracts = registry::contracts(settings.depth)?;
    ensure!(
        contracts.iter().any(|c| c.id.starts_with(&settings.select)),
        "no contracts match {}",
        settings.select
    );
    let output = evidence_directory()?;
    fs::create_dir_all(&output)?;
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&output)?
        .keep();
    eprintln!("Conformance evidence: {}", directory.display());
    write_json(
        &output.join("catalog.json"),
        &contracts
            .iter()
            .map(Contract::catalog)
            .collect::<Result<Vec<_>>>()?,
    )?;
    let (source_sha256, revision) = identity()?;
    let mut report = Report {
        schema: 1,
        settings: &settings,
        source_sha256,
        revision,
        versions: versions()?,
        source_unchanged: false,
        contracts: contracts
            .iter()
            .map(|c| {
                let entry = c.catalog()?;
                Ok(ContractResult {
                    id: c.id,
                    description: entry.description,
                    specification: entry.specification,
                    domain: entry.domain,
                    expected_outcomes: entry.expected_outcomes,
                    status: Status::NotRun,
                    partitions: c
                        .witnesses
                        .iter()
                        .map(|w| PartitionResult {
                            id: w.partition,
                            status: Status::NotRun,
                        })
                        .collect(),
                    generated_cases: 0,
                    executions: 0,
                    error: None,
                })
            })
            .collect::<Result<_>>()?,
    };
    let executor = Executor::new(settings.limits);
    write_json(&output.join("report.json"), &report)?;
    for (index, contract) in contracts.iter().enumerate() {
        let result = &mut report.contracts[index];
        if !contract.id.starts_with(&settings.select) {
            continue;
        }
        if let Some(reason) = crate::deferred(contract.id) {
            result.status = Status::Deferred;
            result.error = Some(reason.to_string());
            eprintln!("{}: deferred ({reason})", contract.id);
            continue;
        }
        result.status = Status::Running;
        let artifacts = directory.join(contract.id);
        fs::create_dir_all(&artifacts)?;
        eprintln!("Checking {}", contract.id);
        executor.begin_typecheck_batch()?;
        for (witness, partition) in contract.witnesses.iter().zip(result.partitions.iter_mut()) {
            let outcome = evaluate(
                contract,
                &witness.case,
                &executor,
                &artifacts.join(witness.partition),
            );
            result.executions += 1;
            if let Err(error) = outcome {
                let _ = executor.flush_typecheck_batch(&artifacts);
                partition.status = Status::Failed;
                result.status = Status::Failed;
                result.error = Some(format!("{error:#}"));
                save_case(
                    &artifacts.join("failure.json"),
                    contract.id,
                    witness.case.clone(),
                    &settings,
                )?;
                break;
            }
            partition.status = Status::Passed;
        }
        if result.status != Status::Failed {
            let attempts = Cell::new(0u32);
            let passed = std::cell::RefCell::new(
                contract
                    .witnesses
                    .iter()
                    .map(|witness| witness.case.clone())
                    .collect::<Vec<_>>(),
            );
            // Shrinking explains regressions; a pending contract only needs its first failure.
            let shrink = if crate::passing(contract.id) {
                settings.shrink_limit
            } else {
                0
            };
            let mut runner = TestRunner::new(Config {
                cases: settings.cases,
                max_shrink_iters: shrink,
                rng_seed: RngSeed::Fixed(settings.seed),
                failure_persistence: None,
                ..Config::default()
            });
            let outcome = runner.run(&contract.strategy, |case| {
                if passed.borrow().contains(&case) {
                    return Ok(());
                }
                let index = attempts.get();
                attempts.set(index + 1);
                evaluate(
                    contract,
                    &case,
                    &executor,
                    &artifacts.join(format!("generated-{index}")),
                )
                .map_err(|error| TestCaseError::fail(format!("{error:#}")))?;
                passed.borrow_mut().push(case);
                Ok(())
            });
            result.executions += attempts.get();
            match outcome {
                Ok(()) => {
                    if let Err(error) = executor.flush_typecheck_batch(&artifacts) {
                        result.status = Status::Failed;
                        result.error = Some(format!("{error:#}"));
                    } else {
                        result.generated_cases = settings.cases;
                        result.status = Status::Passed;
                    }
                }
                Err(error) => {
                    let _ = executor.flush_typecheck_batch(&artifacts);
                    result.status = Status::Failed;
                    result.error = Some(error.to_string());
                    if let TestError::Fail(_, case) = error {
                        save_case(
                            &artifacts.join("minimal.json"),
                            contract.id,
                            case,
                            &settings,
                        )?;
                    }
                }
            }
        }
        // The ratchet: listed contracts must pass, and a newly passing one must be listed.
        match (crate::passing(contract.id), result.status) {
            (false, Status::Failed) => {
                result.status = Status::Pending;
                eprintln!("{}: pending (not in passing.txt)", contract.id);
            }
            (false, Status::Passed) => {
                result.status = Status::Failed;
                result.error = Some(format!(
                    "passes but is not listed; add {} to crates/conformance/passing.txt",
                    contract.id
                ));
            }
            _ => {}
        }
        if let Some(error) = &result.error {
            eprintln!("{}: {error}", contract.id);
        }
    }
    report.source_unchanged = identity()?.0 == report.source_sha256;
    write_json(&directory.join("report.json"), &report)?;
    write_json(&output.join("report.json"), &report)?;
    ensure!(
        report.source_unchanged,
        "source changed during campaign; report cannot establish current evidence"
    );
    ensure!(
        report.contracts.iter().all(|c| c.status != Status::Failed),
        "contract failures; see {}",
        directory.display()
    );
    Ok(directory)
}

pub fn replay(path: &Path) -> Result<()> {
    let saved: SavedCase = serde_json::from_slice(&fs::read(path)?)?;
    saved.settings.validate()?;
    let contracts = registry::contracts(saved.settings.depth)?;
    let contract = contracts
        .iter()
        .find(|c| c.id == saved.contract)
        .context("unknown contract in replay")?;
    let output = evidence_directory()?;
    fs::create_dir_all(&output)?;
    let directory = tempfile::Builder::new()
        .prefix("replay-")
        .tempdir_in(output)?
        .keep();
    let executor = Executor::new(saved.settings.limits);
    let outcome = std::iter::once((&saved.case, &saved.source))
        .chain(saved.metamorphic.iter().map(|v| (&v.case, &v.source)))
        .enumerate()
        .map(|(index, (case, source))| {
            let evidence =
                executor.execute_source(case, source, &directory.join(index.to_string()))?;
            (contract.check)(case, &evidence)?;
            Ok(evidence)
        })
        .collect::<Result<Vec<_>>>()
        .and_then(|evidence| check_metamorphic(&evidence));
    write_json(
        &directory.join("replay.json"),
        &serde_json::json!({"input":saved,"source_identity":identity()?,"versions":versions()?,"passed":outcome.is_ok(),"error":outcome.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    eprintln!("Replay evidence: {}", directory.display());
    outcome
}
