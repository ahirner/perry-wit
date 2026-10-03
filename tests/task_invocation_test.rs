//! Integration tests for direct Canonical ABI task invocation.

#[expect(
    dead_code,
    reason = "This suite uses shared scratch storage with its own host runners."
)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use perry_wit::compiler::{CompileOptions, compile_file, compile_typescript};
use perry_wit::sdk::{SdkOptions, generate_sdk_files};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Engine, Store};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

mod contract_bindings {
    wasmtime::component::bindgen!({path: "wit", world: "repeated-runner"});
}

struct HostState {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

#[test]
fn generated_rust_host_and_guest_declarations_share_the_task_contract() {
    let scratch = support::Scratch::new();
    fs::create_dir_all(scratch.0.join("src")).unwrap();
    let source = r#"
        let saved = "";
        export async function runTask(input: string): Promise<string> { await 0; return input + ":guest"; }
        export function pushItem(item: string): string { saved = item; return saved; }
        export function getHistory(): string { return saved; }
        export function resetHistory(): string { saved = ""; return saved; }
        export function fallibleTask(input: string): {ok: true; value: string} | {ok: false; error: string} {
            if (input === "reject") { return {ok:false, error:"handled"}; }
            return {ok:true, value:input};
        }
    "#;
    let implementation = scratch.0.join("src/index.ts");
    fs::write(&implementation, source).unwrap();
    generate_sdk_files(&SdkOptions {
        wit_dir: PathBuf::from("wit"),
        world: Some("repeated-runner".into()),
        out_dir: scratch.0.join(".perry/types"),
        project_root: Some(scratch.0.clone()),
        entry: PathBuf::from("src/index.ts"),
    })
    .unwrap();
    let check = Command::new("tsc")
        .arg("--noEmit")
        .current_dir(&scratch.0)
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stdout)
    );

    let options = CompileOptions {
        world: Some("repeated-runner".into()),
        ..Default::default()
    };
    let compiled = compile_typescript(source, "host-task.ts", &options).unwrap();
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap()).unwrap();
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker).unwrap();
    let mut store = Store::new(
        &engine,
        HostState {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
        },
    );
    let task =
        contract_bindings::RepeatedRunner::instantiate(&mut store, &component, &linker).unwrap();
    let large = "hé😀\0".repeat(20000);
    for input in ["", "hé😀\0", large.as_str()] {
        for _ in 0..20 {
            assert_eq!(
                task.call_run_task(&mut store, input).unwrap(),
                format!("{input}:guest")
            );
            assert_eq!(task.call_push_item(&mut store, input).unwrap(), input);
            assert_eq!(task.call_get_history(&mut store).unwrap(), input);
            assert_eq!(
                task.call_fallible_task(&mut store, input).unwrap(),
                Ok(input.to_owned())
            );
            assert_eq!(
                task.call_fallible_task(&mut store, "reject").unwrap(),
                Err("handled".into())
            );
            assert_eq!(task.call_reset_history(&mut store).unwrap(), "");
            assert_eq!(task.call_get_history(&mut store).unwrap(), "");
        }
    }

    let wrong_source = source.replace("runTask(input: string)", "runTask(input: number)");
    fs::write(&implementation, &wrong_source).unwrap();
    let check = Command::new("tsc")
        .arg("--noEmit")
        .current_dir(&scratch.0)
        .output()
        .unwrap();
    assert!(!check.status.success());
    assert!(String::from_utf8_lossy(&check.stdout).contains("runTask"));

    let wrong_wit = scratch.0.join("wrong-wit");
    fs::create_dir(&wrong_wit).unwrap();
    fs::write(
        wrong_wit.join("world.wit"),
        fs::read_to_string("wit/world.wit").unwrap().replace(
            "run-task: func(input: string)",
            "run-task: func(input: u32)",
        ),
    )
    .unwrap();
    let wrong = compile_typescript(
        &wrong_source,
        "wrong-task.ts",
        &CompileOptions {
            wit_dir: wrong_wit,
            ..options
        },
    )
    .unwrap();
    let component = Component::new(&engine, wrong.component.unwrap()).unwrap();
    let error = contract_bindings::RepeatedRunner::instantiate(&mut store, &component, &linker)
        .err()
        .expect("host bindings must reject a changed guest signature");
    assert!(format!("{error:#}").contains("run-task"), "{error:#}");
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
