//! Integration tests for direct Canonical ABI task invocation.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use perry_wit::compiler::{CompileOptions, compile_file};

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

    let mut cmd = get_wasmtime_cmd();
    let output = cmd
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
        .expect("Running wasmtime");

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

    let mut cmd1 = get_wasmtime_cmd();
    let output1 = cmd1
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
        .expect("Running wasmtime for run-task");

    let stdout1 = String::from_utf8_lossy(&output1.stdout);
    assert!(
        output1.status.success(),
        "run-task failed:\n{}",
        String::from_utf8_lossy(&output1.stderr)
    );
    assert!(stdout1.contains("TASK_PROCESSED: doc1"));

    // 2. Invoke merge-task
    let mut cmd2 = get_wasmtime_cmd();
    let output2 = cmd2
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
        .expect("Running wasmtime for merge-task");

    let stdout2 = String::from_utf8_lossy(&output2.stdout);
    assert!(
        output2.status.success(),
        "merge-task failed:\n{}",
        String::from_utf8_lossy(&output2.stderr)
    );
    assert!(stdout2.contains("MERGED_DOCUMENT: payload"));
}
