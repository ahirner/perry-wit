//! Campaign orchestration and structured evidence; no parsing of test-runner output.
use crate::{
    execution::{Executor, Limits},
    registry::{self, Case, Contract},
};
use anyhow::{Context, Result, ensure};
use proptest::test_runner::{Config, RngSeed, TestCaseError, TestError, TestRunner};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    cell::Cell,
    fs,
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
    pub fn validate(&self) -> Result<()> {
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
pub enum Status {
    NotRun,
    Running,
    Passed,
    Failed,
}
#[derive(Serialize)]
pub struct PartitionResult {
    pub id: &'static str,
    pub status: Status,
}
#[derive(Serialize)]
pub struct ContractResult {
    pub id: &'static str,
    pub status: Status,
    pub partitions: Vec<PartitionResult>,
    pub generated_cases: u32,
    pub executions: u32,
    pub error: Option<String>,
}
#[derive(Serialize)]
pub struct Report {
    pub schema: u32,
    pub settings: Settings,
    pub source_sha256: String,
    pub revision: String,
    pub versions: serde_json::Value,
    pub contracts: Vec<ContractResult>,
    pub source_unchanged: bool,
}
#[derive(Serialize, Deserialize)]
pub struct SavedCase {
    pub contract: String,
    pub case: Case,
    pub source: String,
    pub settings: Settings,
    pub source_sha256: String,
    pub revision: String,
    pub versions: serde_json::Value,
}

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}
fn identity() -> Result<(String, String)> {
    let root = root();
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
    Ok(
        serde_json::json!({"node":version("node")?,"typescript":version("tsc")?,"wasmtime":version("wasmtime")?,"rust":version("rustc")?}),
    )
}
fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(value)?)?;
    fs::rename(temporary, path)?;
    Ok(())
}
fn evaluate(contract: &Contract, case: &Case, executor: &Executor, directory: &Path) -> Result<()> {
    for evidence in executor.check(case, directory)? {
        (contract.check)(case, &evidence)?;
    }
    Ok(())
}
fn save_case(path: &Path, contract: &str, case: Case, settings: &Settings) -> Result<()> {
    fs::write(path.with_extension("ts"), case.source())?;
    let (source_sha256, revision) = identity()?;
    write_json(
        path,
        &SavedCase {
            contract: contract.into(),
            source: case.source(),
            case,
            settings: settings.clone(),
            source_sha256,
            revision,
            versions: versions()?,
        },
    )
}

pub fn campaign(settings: Settings, executable: PathBuf) -> Result<PathBuf> {
    settings.validate()?;
    let contracts = registry::contracts(settings.depth)?;
    ensure!(
        contracts.iter().any(|c| c.id.starts_with(&settings.select)),
        "no contracts match {}",
        settings.select
    );
    let output = root().join("target/conformance");
    fs::create_dir_all(&output)?;
    let directory = tempfile::Builder::new()
        .prefix("run-")
        .tempdir_in(&output)?
        .keep();
    eprintln!("Conformance evidence: {}", directory.display());
    write_json(
        &output.join("catalog.json"),
        &contracts.iter().map(Contract::catalog).collect::<Vec<_>>(),
    )?;
    let (source_sha256, revision) = identity()?;
    let mut report = Report {
        schema: 1,
        settings: settings.clone(),
        source_sha256,
        revision,
        versions: versions()?,
        source_unchanged: false,
        contracts: contracts
            .iter()
            .map(|c| ContractResult {
                id: c.id,
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
            .collect(),
    };
    let executor = Executor::new(executable, settings.limits.clone());
    write_json(&output.join("report.json"), &report)?;
    for (index, contract) in contracts.iter().enumerate() {
        let result = &mut report.contracts[index];
        if !contract.id.starts_with(&settings.select) {
            continue;
        }
        result.status = Status::Running;
        let artifacts = directory.join(contract.id);
        fs::create_dir_all(&artifacts)?;
        eprintln!("Checking {}", contract.id);
        for (witness, partition) in contract.witnesses.iter().zip(result.partitions.iter_mut()) {
            let outcome = evaluate(
                contract,
                &witness.case,
                &executor,
                &artifacts.join(witness.partition),
            );
            result.executions += 1;
            if let Err(error) = outcome {
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
            let mut runner = TestRunner::new(Config {
                cases: settings.cases,
                max_shrink_iters: settings.shrink_limit,
                rng_seed: RngSeed::Fixed(settings.seed),
                failure_persistence: None,
                ..Config::default()
            });
            let outcome = runner.run(&contract.strategy, |case| {
                let index = attempts.get();
                attempts.set(index + 1);
                evaluate(
                    contract,
                    &case,
                    &executor,
                    &artifacts.join(format!("generated-{index}")),
                )
                .map_err(|error| TestCaseError::fail(format!("{error:#}")))
            });
            result.executions += attempts.get();
            match outcome {
                Ok(()) => {
                    result.generated_cases = settings.cases;
                    result.status = Status::Passed;
                }
                Err(error) => {
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
        if let Some(error) = &result.error {
            eprintln!("{}: {error}", contract.id);
        }
        write_json(&directory.join("report.json"), &report)?;
        write_json(&output.join("report.json"), &report)?;
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

pub fn replay(path: &Path, executable: PathBuf) -> Result<()> {
    let saved: SavedCase = serde_json::from_slice(&fs::read(path)?)?;
    saved.settings.validate()?;
    ensure!(
        saved.source == saved.case.source(),
        "saved source does not match its concrete case"
    );
    let contracts = registry::contracts(saved.settings.depth)?;
    let contract = contracts
        .iter()
        .find(|c| c.id == saved.contract)
        .context("unknown contract in replay")?;
    let output = root().join("target/conformance");
    fs::create_dir_all(&output)?;
    let directory = tempfile::Builder::new()
        .prefix("replay-")
        .tempdir_in(output)?
        .keep();
    let outcome = evaluate(
        contract,
        &saved.case,
        &Executor::new(executable, saved.settings.limits.clone()),
        &directory,
    );
    write_json(
        &directory.join("replay.json"),
        &serde_json::json!({"input":saved,"source_identity":identity()?,"versions":versions()?,"passed":outcome.is_ok(),"error":outcome.as_ref().err().map(|e|format!("{e:#}"))}),
    )?;
    eprintln!("Replay evidence: {}", directory.display());
    outcome
}
