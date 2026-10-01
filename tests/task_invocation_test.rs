//! Integration tests for direct Canonical ABI task invocation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use perry_wit::compiler::{CompileOptions, compile_file};

#[test]
fn test_task_component_direct_invocation() {
    let out_dir = std::env::temp_dir().join("perry_wit_task_test");
    let _ = fs::create_dir_all(&out_dir);
    let wasm_path = out_dir.join("merge_task.wasm");

    let options = CompileOptions {
        out_path: Some(wasm_path.clone()),
        runtime_path: None,
        wit_dir: PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        core_only: false,
    };

    let compiled = compile_file(Path::new("examples/merge_task.ts"), &options)
        .expect("Compilation of merge_task.ts to WebAssembly component failed");

    assert!(compiled.component.is_some());
    let comp_bytes = compiled.component.unwrap();
    fs::write(&wasm_path, &comp_bytes).unwrap();

    // Check direct invocation using wasmtime
    let direct_wasmtime = Command::new("wasmtime")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let output = if direct_wasmtime {
        Command::new("wasmtime")
            .args([
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "run-task(\"hello-component-task\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime")
    } else {
        Command::new("nix")
            .args([
                "develop",
                "--command",
                "wasmtime",
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "run-task(\"hello-component-task\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime via nix develop")
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    println!("STDOUT: {stdout}");
    println!("STDERR: {stderr}");
    println!("STATUS: {:?}", output.status);

    assert!(
        output.status.success(),
        "wasmtime invocation failed with status {:?}:\nStderr: {stderr}\nStdout: {stdout}",
        output.status
    );
    assert!(
        stdout.contains("TASK_PROCESSED: hello-component-task"),
        "Expected output containing 'TASK_PROCESSED: hello-component-task', got: {stdout}"
    );
}

#[test]
fn test_multi_task_component_direct_invocation() {
    let out_dir = std::env::temp_dir().join("perry_wit_multi_task_test");
    let _ = fs::create_dir_all(&out_dir);
    let wasm_path = out_dir.join("merge_task_multi.wasm");

    let options = CompileOptions {
        out_path: Some(wasm_path.clone()),
        runtime_path: None,
        wit_dir: PathBuf::from("wit"),
        world: Some("merge-task".to_string()),
        core_only: false,
    };

    let compiled = compile_file(Path::new("examples/merge_task.ts"), &options)
        .expect("Compilation of merge_task.ts to WebAssembly component failed");

    assert!(compiled.component.is_some());
    let comp_bytes = compiled.component.unwrap();
    fs::write(&wasm_path, &comp_bytes).unwrap();

    let direct_wasmtime = Command::new("wasmtime")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    // 1. Invoke run-task
    let output1 = if direct_wasmtime {
        Command::new("wasmtime")
            .args([
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "run-task(\"doc1\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime for run-task")
    } else {
        Command::new("nix")
            .args([
                "develop",
                "--command",
                "wasmtime",
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "run-task(\"doc1\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime via nix develop")
    };

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    assert!(
        output1.status.success(),
        "run-task failed:\n{}",
        String::from_utf8_lossy(&output1.stderr)
    );
    assert!(stdout1.contains("TASK_PROCESSED: doc1"));

    // 2. Invoke merge-task
    let output2 = if direct_wasmtime {
        Command::new("wasmtime")
            .args([
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "merge-task(\"payload\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime for merge-task")
    } else {
        Command::new("nix")
            .args([
                "develop",
                "--command",
                "wasmtime",
                "run",
                "-S",
                "http=y",
                "-S",
                "inherit-network=y",
                "--invoke",
                "merge-task(\"payload\")",
            ])
            .arg(&wasm_path)
            .output()
            .expect("Running wasmtime via nix develop")
    };

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "merge-task failed:\n{}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(stdout2.contains("MERGED_DOCUMENT: payload"));
}
