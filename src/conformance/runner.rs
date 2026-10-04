//! Pure-Rust differential execution harness comparing Node.js reference oracle vs Perry WASI 0.3 components.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::output::{filter_nix_banner, normalize_stream};
use crate::compiler::{CompileOptions, compile_file};

/// Captured execution vector from an engine run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionVector {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Differential comparison result between Node.js oracle and Perry WASI 0.3 SUT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ComparisonResult {
    pub case_path: String,
    pub matched: bool,
    pub oracle: ExecutionVector,
    pub wasm: ExecutionVector,
    pub discrepancies: Vec<String>,
}

/// Executes a TypeScript file under the native Node.js reference oracle.
pub fn run_node_oracle(script_path: &Path) -> Result<ExecutionVector> {
    let output = Command::new("node")
        .args(["--input-type=module", "--eval", "import { pathToFileURL } from 'node:url'; const task = await import(pathToFileURL(process.argv[1])); const result = await task.runRun(); if (!result.ok) process.exitCode = 1;"])
        .arg(fs::canonicalize(script_path)?)
        .env("PERRY_CONFORMANCE", "fixture")
        .output()
        .with_context(|| format!("Failed to execute node for {}", script_path.display()))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8(output.stdout)?;
    let stderr = String::from_utf8(output.stderr)?;

    Ok(ExecutionVector {
        exit_code,
        stdout,
        stderr,
    })
}

fn get_wasmtime_cmd() -> Command {
    if let Ok(path) = std::env::var("WASMTIME") {
        return Command::new(path);
    }
    if Command::new("wasmtime").arg("--version").output().is_ok() {
        return Command::new("wasmtime");
    }
    let mut cmd = Command::new("nix");
    cmd.args(["develop", "--command", "wasmtime"]);
    cmd
}

/// Executes a compiled WASI 0.3 component under wasmtime.
pub fn run_wasmtime(wasm_path: &Path) -> Result<ExecutionVector> {
    let mut cmd = get_wasmtime_cmd();
    let output = cmd
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
        .arg(wasm_path)
        .output()
        .with_context(|| format!("Failed to run wasmtime for {}", wasm_path.display()))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let raw_stdout = String::from_utf8(output.stdout)?;
    let raw_stderr = String::from_utf8(output.stderr)?;

    // Filter out nix develop banner if present
    let stdout = filter_nix_banner(&raw_stdout);
    let stderr = filter_nix_banner(&raw_stderr);

    Ok(ExecutionVector {
        exit_code,
        stdout,
        stderr,
    })
}

/// Compiles and differentially executes a single conformance test case.
pub fn execute_conformance_case(case_path: &Path, scratch_dir: &Path) -> Result<ComparisonResult> {
    ensure!(
        case_path.exists(),
        "Case file not found: {}",
        case_path.display()
    );

    let case_stem = case_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("case");
    let target_wasm = scratch_dir.join(format!("{case_stem}.wasm"));

    let options = CompileOptions::default();
    let compiled = compile_file(case_path, &options)
        .with_context(|| format!("Compiling {} to WASI 0.3", case_path.display()))?;

    let component_bytes = compiled
        .stripped
        .or(compiled.component)
        .context("Missing component output from compilation")?;

    fs::write(&target_wasm, component_bytes)?;

    let oracle = run_node_oracle(case_path)?;
    let wasm = run_wasmtime(&target_wasm)?;

    let mut discrepancies = Vec::new();

    // 1. Exit code parity check
    if oracle.exit_code != wasm.exit_code {
        discrepancies.push(format!(
            "Exit code mismatch: node={}, wasm={}",
            oracle.exit_code, wasm.exit_code
        ));
    }

    // 2. Output comparison
    if oracle.exit_code == 0 && wasm.exit_code == 0 {
        // Successful runs: verify stdout equivalence
        if let (Ok(json_node), Ok(json_wasm)) = (
            serde_json::from_str::<serde_json::Value>(&oracle.stdout),
            serde_json::from_str::<serde_json::Value>(&wasm.stdout),
        ) {
            if json_node != json_wasm {
                discrepancies.push(format!(
                    "Structured JSON mismatch:\nNode: {json_node}\nWasm: {json_wasm}"
                ));
            }
        } else {
            // Compare normalized text lines
            let norm_node = normalize_stream(&oracle.stdout);
            let norm_wasm = normalize_stream(&wasm.stdout);
            if norm_node != norm_wasm {
                discrepancies.push(format!(
                    "Stdout stream mismatch:\nNode: {:?}\nWasm: {:?}",
                    norm_node, norm_wasm
                ));
            }
        }

        // Compare stderr stream routing (e.g. console.error)
        let norm_err_node = normalize_stream(&oracle.stderr);
        let norm_err_wasm = normalize_stream(&wasm.stderr);
        if norm_err_node != norm_err_wasm {
            discrepancies.push(format!(
                "Stderr stream mismatch:\nNode: {:?}\nWasm: {:?}",
                norm_err_node, norm_err_wasm
            ));
        }
    } else {
        // Failing runs: verify host invariants and diagnostic emission
        if wasm.stderr.is_empty() {
            discrepancies.push(
                "Expected non-empty error diagnostic on stderr for failed execution".to_string(),
            );
        }
        if wasm.stderr.contains("wasm trap: unreachable") || wasm.stderr.contains("out of bounds") {
            discrepancies.push(
                "Host invariant violation: memory trap or host panic encountered".to_string(),
            );
        }
    }

    let matched = discrepancies.is_empty();

    Ok(ComparisonResult {
        case_path: case_path.to_string_lossy().to_string(),
        matched,
        oracle,
        wasm,
        discrepancies,
    })
}

/// Executes all conformance test cases in a given directory against the catalog.
pub fn run_conformance_suite(
    cases_dir: &Path,
    scratch_dir: &Path,
) -> Result<Vec<ComparisonResult>> {
    let mut results = Vec::new();
    let mut entries: Vec<PathBuf> = fs::read_dir(cases_dir)
        .with_context(|| format!("Reading cases dir {}", cases_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("ts"))
        .collect();

    entries.sort();

    for case_path in entries {
        let res = execute_conformance_case(&case_path, scratch_dir)?;
        results.push(res);
    }

    Ok(results)
}
