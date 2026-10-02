//! Tests for Phase 8: WASI Preview 2 Environment & Arguments (`wasi:cli`).

mod support;

use std::process::Command;

#[test]
fn pure_component_prunes_environment_import() {
    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(
        r#"
        export function compute(): number {
            return 42;
        }
    "#,
        None,
    );
    let module = wasmparser::Parser::new(0);
    let mut imported_modules = Vec::new();
    for payload in module.parse_all(&compiled.core) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            for import in reader.into_imports() {
                let import = import.unwrap();
                imported_modules.push(import.module.to_string());
            }
        }
    }
    assert!(
        !imported_modules
            .iter()
            .any(|m| m.contains("wasi:cli/environment")),
        "Pure components must prune wasi:cli/environment imports, found: {imported_modules:?}"
    );
}

#[test]
fn process_env_only_component_retains_environment_and_prunes_http_and_clocks() {
    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(
        r#"
        export function getVar(): string {
            return process.env.MY_VAR ?? "fallback";
        }
    "#,
        None,
    );
    let module = wasmparser::Parser::new(0);
    let mut imported_modules = Vec::new();
    for payload in module.parse_all(&compiled.core) {
        if let Ok(wasmparser::Payload::ImportSection(reader)) = payload {
            for import in reader.into_imports() {
                let import = import.unwrap();
                imported_modules.push(import.module.to_string());
            }
        }
    }
    assert!(
        imported_modules
            .iter()
            .any(|m| m.contains("wasi:cli/environment")),
        "Component using process.env must import wasi:cli/environment, found: {imported_modules:?}"
    );
    assert!(
        !imported_modules
            .iter()
            .any(|m| m.contains("wasi:http")),
        "Component using only process.env must prune wasi:http, found: {imported_modules:?}"
    );
    assert!(
        !imported_modules
            .iter()
            .any(|m| m.contains("wasi:clocks")),
        "Component using only process.env must prune wasi:clocks, found: {imported_modules:?}"
    );
    assert!(
        !imported_modules
            .iter()
            .any(|m| m.contains("wasi:random")),
        "Component using only process.env must prune wasi:random, found: {imported_modules:?}"
    );
}

#[test]
fn process_env_and_argv_execution_under_wasmtime() {
    let scratch = support::Scratch::new();
    let wasm = scratch.compile(
        r#"
        console.log("FOO=" + (process.env.FOO ?? "missing"));
        console.log("BAR=" + (process.env["BAR"] ?? "missing"));
        console.log("EMPTY=" + (process.env.EMPTY_VAR === "" ? "is_empty" : "not_empty"));
        console.log("NONEXISTENT=" + (process.env.NONEXISTENT === undefined ? "is_undefined" : "defined"));

        // Mutation test
        process.env.DYNAMIC_SET = "custom_value";
        console.log("DYNAMIC=" + process.env.DYNAMIC_SET);

        // Argv test
        console.log("ARGV_LEN=" + process.argv.length);
        if (process.argv.length > 1) {
            console.log("ARG1=" + process.argv[1]);
        }
        if (process.argv.length > 2) {
            console.log("ARG2=" + process.argv[2]);
        }

        // CWD test
        const cwd = process.cwd();
        console.log("CWD_EXISTS=" + (typeof cwd === "string" && cwd.length > 0));
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--env",
        "FOO=hello_wasm",
        "--env",
        "BAR=world_preview2",
        "--env",
        "EMPTY_VAR=",
        wasm.to_str().unwrap(),
        "first_arg",
        "second_arg",
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(stdout.contains("FOO=hello_wasm"), "stdout: {stdout}");
    assert!(stdout.contains("BAR=world_preview2"), "stdout: {stdout}");
    assert!(stdout.contains("EMPTY=is_empty"), "stdout: {stdout}");
    assert!(stdout.contains("NONEXISTENT=is_undefined"), "stdout: {stdout}");
    assert!(stdout.contains("DYNAMIC=custom_value"), "stdout: {stdout}");
    assert!(stdout.contains("ARGV_LEN=3"), "stdout: {stdout}");
    assert!(stdout.contains("ARG1=first_arg"), "stdout: {stdout}");
    assert!(stdout.contains("ARG2=second_arg"), "stdout: {stdout}");
    assert!(stdout.contains("CWD_EXISTS=true"), "stdout: {stdout}");
}

#[test]
fn process_env_object_keys_and_json_stringify() {
    let scratch = support::Scratch::new();
    let wasm = scratch.compile(
        r#"
        process.env.ALPHA = "one";
        process.env.BETA = "two";

        const keys = Object.keys(process.env);
        console.log("HAS_ALPHA=" + keys.includes("ALPHA"));
        console.log("HAS_BETA=" + keys.includes("BETA"));

        const json = JSON.stringify(process.env);
        console.log("JSON_HAS_ALPHA=" + json.includes('"ALPHA":"one"'));
        console.log("JSON_HAS_BETA=" + json.includes('"BETA":"two"'));
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(stdout.contains("HAS_ALPHA=true"), "stdout: {stdout}");
    assert!(stdout.contains("HAS_BETA=true"), "stdout: {stdout}");
    assert!(stdout.contains("JSON_HAS_ALPHA=true"), "stdout: {stdout}");
    assert!(stdout.contains("JSON_HAS_BETA=true"), "stdout: {stdout}");
}
