//! Pure-Rust differential execution harness comparing Node.js reference oracle vs Perry WASIp2 components.

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::compiler::{CompileOptions, compile_file};

/// Captured execution vector from an engine run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionVector {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Differential comparison result between Node.js oracle and Perry WASIp2 SUT.
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
        .arg(script_path)
        .output()
        .with_context(|| format!("Failed to execute node for {}", script_path.display()))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

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
    if let Ok(entries) = std::fs::read_dir("/nix/store") {
        for entry in entries.flatten() {
            let path = entry.path().join("bin/wasmtime");
            if path.exists() {
                return Command::new(path);
            }
        }
    }
    let mut cmd = Command::new("nix");
    cmd.args(["develop", "--command", "wasmtime"]);
    cmd
}

/// Executes a compiled WASIp2 component under wasmtime.
pub fn run_wasmtime(wasm_path: &Path) -> Result<ExecutionVector> {
    let mut cmd = get_wasmtime_cmd();
    let output = cmd
        .args(["run", "-S", "http=y", "-S", "inherit-network=y"])
        .arg(wasm_path)
        .output()
        .with_context(|| format!("Failed to run wasmtime for {}", wasm_path.display()))?;

    let exit_code = output.status.code().unwrap_or(-1);
    let raw_stdout = String::from_utf8_lossy(&output.stdout);
    let raw_stderr = String::from_utf8_lossy(&output.stderr);

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
        .with_context(|| format!("Compiling {} to WASIp2", case_path.display()))?;

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

/// Embedded mock HTTP server guard for hermetic test isolation.
pub struct MockServerGuard {
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServerGuard {
    /// Starts the mock HTTP server on 127.0.0.1:8080 if not already running.
    pub fn start_if_needed() -> Result<Self> {
        let shutdown = Arc::new(AtomicBool::new(false));

        match TcpListener::bind("127.0.0.1:8080") {
            Ok(listener) => {
                let listener_clone = listener.try_clone()?;
                let shutdown_clone = Arc::clone(&shutdown);

                let handle = thread::spawn(move || {
                    for mut stream in listener_clone.incoming().flatten() {
                        if shutdown_clone.load(Ordering::Relaxed) {
                            break;
                        }
                        thread::spawn(move || {
                            let mut buf = [0u8; 2048];
                            if let Ok(n) = stream.read(&mut buf)
                                && n > 0
                            {
                                let req = String::from_utf8_lossy(&buf[..n]);
                                let (status, content_type, body) = if req.contains("GET /doc1.json")
                                {
                                    (
                                        "200 OK",
                                        "application/json",
                                        fs::read_to_string("examples/doc1.json")
                                            .unwrap_or_default(),
                                    )
                                } else if req.contains("GET /doc2.json") {
                                    (
                                        "200 OK",
                                        "application/json",
                                        fs::read_to_string("examples/doc2.json")
                                            .unwrap_or_default(),
                                    )
                                } else if req.contains("GET /invalid.json") {
                                    (
                                        "200 OK",
                                        "application/json",
                                        "{ this is malformed json".to_string(),
                                    )
                                } else {
                                    ("404 NOT FOUND", "text/plain", "Not Found".to_string())
                                };

                                let resp = format!(
                                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                    body.len(),
                                    body
                                );
                                let _ = stream.write_all(resp.as_bytes());
                                let _ = stream.flush();
                            }
                        });
                    }
                });

                Ok(Self {
                    shutdown,
                    handle: Some(handle),
                })
            }
            Err(_) => {
                // Port already bound; assume existing mock server is active
                Ok(Self {
                    shutdown,
                    handle: None,
                })
            }
        }
    }
}

impl Drop for MockServerGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // Ping socket to unblock incoming() iterator
        let _ = std::net::TcpStream::connect("127.0.0.1:8080");
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Executes all conformance test cases in a given directory against the catalog.
pub fn run_conformance_suite(
    cases_dir: &Path,
    scratch_dir: &Path,
) -> Result<Vec<ComparisonResult>> {
    let _server = MockServerGuard::start_if_needed()?;

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

fn filter_nix_banner(s: &str) -> String {
    let mut out = Vec::new();
    let mut in_banner = false;
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.contains("=== Perry-WIT Hermetic Environment ===")
            || trimmed.contains("Perry-WIT compiler development")
            || trimmed.starts_with("Perry-WIT")
        {
            in_banner = true;
            continue;
        }
        if in_banner {
            if trimmed.contains("======================================") {
                in_banner = false;
            }
            continue;
        }
        if trimmed.starts_with("warning:")
            || trimmed.starts_with("building '/nix/store")
            || trimmed.starts_with("evaluating flake")
            || trimmed.starts_with("copying path '/nix/store")
        {
            continue;
        }
        out.push(line);
    }
    out.join("\n").trim().to_string()
}

fn normalize_stream(s: &str) -> Vec<String> {
    s.lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}
