//! Tests for Phase 9.1: Sandboxed Filesystem (`wasi:filesystem`).

mod support;

use std::fs;
use std::process::Command;

#[test]
fn unsupported_write_options_never_modify_or_create_files() {
    let scratch = support::Scratch::new();
    let directory = scratch.0.join("sandbox");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("existing.txt"), "original").unwrap();
    let wasm = scratch.compile(
        r#"
        import { writeFileSync } from "node:fs";
        let evaluations = 0;
        function options(flag: string) { evaluations++; return {flag: flag}; }
        const rejected = [options("wx"), options("a"), {encoding: "hex"}, {mode: 384}];
        for (let i = 0; i < rejected.length; i++) {
            try {
                writeFileSync("/sandbox/existing.txt", "new", rejected[i]);
                console.log("unreachable");
            } catch (error) { console.log("caught"); }
            try {
                writeFileSync("/sandbox/missing.txt", "new", rejected[i]);
                console.log("unreachable");
            } catch (error) { console.log("caught"); }
        }
        console.log(evaluations);
        writeFileSync("/sandbox/valid.txt", "initial", "utf8");
        writeFileSync("/sandbox/valid.txt", "é", {encoding: "utf-8", flag: "w"});
    "#,
        None,
    );
    let output = Command::new(support::get_wasmtime_path())
        .args([
            "run",
            "-C",
            "cache=n",
            "--dir",
            &format!("{}::/sandbox", directory.display()),
        ])
        .arg(wasm)
        .output()
        .unwrap();
    assert_eq!(
        support::stdout(&output),
        format!("{}2\n", "caught\n".repeat(8))
    );
    assert_eq!(
        fs::read_to_string(directory.join("existing.txt")).unwrap(),
        "original"
    );
    assert!(!directory.join("missing.txt").exists());
    assert_eq!(
        fs::read_to_string(directory.join("valid.txt")).unwrap(),
        "é"
    );
    let output = support::run(
        r#"
        import * as fs from "fs";
        try { fs.writeFileSync("/no-preopen/file", "new", {flag: "wx"}); }
        catch (error) { console.log(error); }
    "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: Unsupported writeFileSync options; only UTF-8 encoding and flag 'w' are supported\n"
    );
}

#[test]
fn pure_component_prunes_filesystem_imports() {
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
            .any(|m| m.contains("wasi:filesystem")),
        "Pure components must prune wasi:filesystem imports, found: {imported_modules:?}"
    );
}

#[test]
fn filesystem_component_retains_filesystem_and_prunes_http_and_clocks() {
    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(
        r#"
        import * as fs from "fs";

        export function readFile(path: string): string {
            return fs.readFileSync(path, "utf8");
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
            .any(|m| m.contains("wasi:filesystem/types")),
        "Component using fs must import wasi:filesystem/types, found: {imported_modules:?}"
    );
    assert!(
        imported_modules
            .iter()
            .any(|m| m.contains("wasi:filesystem/preopens")),
        "Component using fs must import wasi:filesystem/preopens, found: {imported_modules:?}"
    );
    assert!(
        !imported_modules.iter().any(|m| m.contains("wasi:http")),
        "Component using only fs must prune wasi:http, found: {imported_modules:?}"
    );
    assert!(
        !imported_modules.iter().any(|m| m.contains("wasi:clocks")),
        "Component using only fs must prune wasi:clocks, found: {imported_modules:?}"
    );
}

#[test]
fn fs_read_write_roundtrip_execution_under_wasmtime() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        const filePath = "/sandbox/test.txt";
        const message = "Hello from WASI Preview 2 Filesystem!";

        fs.writeFileSync(filePath, message);
        console.log("WRITE_DONE");

        const readBack = fs.readFileSync(filePath, "utf8");
        console.log("READ_BACK=" + readBack);

        // Append / overwrite test
        fs.writeFileSync(filePath, message + " - updated");
        const updated = fs.readFileSync(filePath, "utf8");
        console.log("UPDATED=" + updated);
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(stdout.contains("WRITE_DONE"), "stdout: {stdout}");
    assert!(
        stdout.contains("READ_BACK=Hello from WASI Preview 2 Filesystem!"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("UPDATED=Hello from WASI Preview 2 Filesystem! - updated"),
        "stdout: {stdout}"
    );

    // Verify on host filesystem as well
    let on_host = fs::read_to_string(host_dir.join("test.txt")).unwrap();
    assert_eq!(on_host, "Hello from WASI Preview 2 Filesystem! - updated");
}

#[test]
fn fs_named_import_execution_under_wasmtime() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import { readFileSync, writeFileSync } from "node:fs";

        writeFileSync("/sandbox/named.txt", "Named import content");
        const readBack = readFileSync("/sandbox/named.txt", "utf8");
        console.log("NAMED_READ=" + readBack);
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(
        stdout.contains("NAMED_READ=Named import content"),
        "stdout: {stdout}"
    );
}

#[test]
fn fs_missing_file_throws_enoent() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        let caught = "";
        try {
            fs.readFileSync("/sandbox/nonexistent.txt", "utf8");
        } catch (e: any) {
            caught = "" + e;
        }
        console.log("CAUGHT=" + caught);
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(
        stdout.contains("CAUGHT=Error: ENOENT: no such file or directory"),
        "stdout: {stdout}"
    );
}

#[test]
fn fs_path_escape_outside_preopen_throws_eacces() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        let rootCaught = "";
        try {
            // Path completely outside preopens
            fs.readFileSync("/etc/passwd", "utf8");
        } catch (e: any) {
            rootCaught = "" + e;
        }
        console.log("ROOT_CAUGHT=" + rootCaught);

        let escapeCaught = "";
        try {
            // Path attempting dot-dot escape
            fs.readFileSync("/sandbox/../../outside.txt", "utf8");
        } catch (e: any) {
            escapeCaught = "" + e;
        }
        console.log("ESCAPE_CAUGHT=" + escapeCaught);
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(
        stdout.contains("ROOT_CAUGHT=Error: EACCES: permission denied"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("ESCAPE_CAUGHT=Error: EACCES: permission denied"),
        "stdout: {stdout}"
    );
}

#[test]
fn fs_large_multi_chunk_read_write() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        // Generate 128 KB of repeating text
        let chunk = "0123456789abcdef"; // 16 bytes
        let str = "";
        for (let i = 0; i < 8192; i++) {
            str += chunk;
        }

        fs.writeFileSync("/sandbox/large.txt", str);
        console.log("WRITE_LARGE_DONE");

        const readBack = fs.readFileSync("/sandbox/large.txt", "utf8");
        console.log("READ_LEN=" + readBack.length);
        console.log("EQUAL=" + (readBack === str));
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(stdout.contains("WRITE_LARGE_DONE"), "stdout: {stdout}");
    assert!(stdout.contains("READ_LEN=131072"), "stdout: {stdout}");
    assert!(stdout.contains("EQUAL=true"), "stdout: {stdout}");
}

#[test]
fn fs_repeated_operations_no_descriptor_leakage() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        // Perform 500 write and read cycles to ensure descriptor drops succeed
        for (let i = 0; i < 500; i++) {
            const path = "/sandbox/cycle_" + (i % 10) + ".txt";
            fs.writeFileSync(path, "iteration " + i);
            const content = fs.readFileSync(path, "utf8");
            if (content !== "iteration " + i) {
                throw new Error("Mismatch at iteration " + i);
            }
        }
        console.log("CYCLES_PASSED=500");
    "#,
        None,
    );

    let mut command = Command::new(support::get_wasmtime_path());
    command.args([
        "run",
        "-C",
        "cache=n",
        "--dir",
        &format!("{}::/sandbox", host_dir.display()),
        wasm.to_str().unwrap(),
    ]);

    let output = command.output().expect("wasmtime execution failed");
    let stdout = support::stdout(&output);

    assert!(stdout.contains("CYCLES_PASSED=500"), "stdout: {stdout}");
}
