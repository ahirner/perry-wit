//! Tests for Phase 9.1: Sandboxed Filesystem (`wasi:filesystem`).

mod support;

use std::fs;
use std::process::Command;

#[test]
fn mkdir_options_are_evaluated_and_rejected_before_directory_creation() {
    for (import, mkdir) in [
        ("import * as fs from 'fs';", "fs.mkdirSync"),
        ("import { mkdirSync } from 'node:fs';", "mkdirSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox/rejected" + index;
            }
            function options(index: number) {
                evaluations += "options;";
                return [0o700, {mode: 0o700}, {recursive: true}, {recursive: false, mode: 0o700}][index];
            }
            for (let i = 0; i < 4; i++) {
                try {
                    MKDIR(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);
            MKDIR("/sandbox/default");
            MKDIR("/sandbox/explicit", undefined);
        "#
        .replace("IMPORT", import)
        .replace("MKDIR", mkdir);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        assert_eq!(
            support::stdout(&output),
            format!(
                "{}{}\n",
                "TypeError: mkdirSync options are not supported\n".repeat(4),
                "path;options;".repeat(4)
            ),
            "{import}"
        );
        for index in 0..4 {
            assert!(!directory.join(format!("rejected{index}")).exists());
        }
        assert!(directory.join("default").is_dir());
        assert!(directory.join("explicit").is_dir());
    }
    let output = support::run(
        r#"
            import { mkdirSync } from "fs";
            try { mkdirSync("/no-preopen/private", 0o700); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: mkdirSync options are not supported\n"
    );
}

#[test]
fn readdir_options_are_evaluated_and_rejected_before_reading() {
    for (import, readdir) in [
        ("import * as fs from 'fs';", "fs.readdirSync"),
        ("import { readdirSync } from 'node:fs';", "readdirSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("f1.txt"), "1").unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox";
            }
            function options(index: number) {
                evaluations += "options;";
                return [{withFileTypes: true}, {recursive: true}, "hex", 0o700][index];
            }
            for (let i = 0; i < 4; i++) {
                try {
                    READDIR(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);

            // Supported options:
            const r1 = READDIR("/sandbox");
            const r2 = READDIR("/sandbox", undefined);
            const r3 = READDIR("/sandbox", "utf8");
            const r4 = READDIR("/sandbox", "utf-8");
            const r5 = READDIR("/sandbox", { encoding: "utf8" });
            const r6 = READDIR("/sandbox", { withFileTypes: false, recursive: false });
            console.log(r1.length + "," + r2.length + "," + r3.length + "," + r4.length + "," + r5.length + "," + r6.length);
        "#
        .replace("IMPORT", import)
        .replace("READDIR", readdir);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        let stdout = support::stdout(&output);
        assert!(stdout.contains("path;options;path;options;path;options;path;options;"), "{import}");
        assert!(stdout.contains("1,1,1,1,1,1"), "{import}");
    }
    let output = support::run(
        r#"
            import { readdirSync } from "fs";
            try { readdirSync("/no-preopen/private", { recursive: true }); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: readdirSync options are not supported\n"
    );
}

#[test]
fn stat_options_are_evaluated_and_rejected_before_stat() {
    for (import, stat) in [
        ("import * as fs from 'fs';", "fs.statSync"),
        ("import { statSync } from 'node:fs';", "statSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("f1.txt"), "hello").unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox/f1.txt";
            }
            function options(index: number) {
                evaluations += "options;";
                return [{bigint: true}, {throwIfNoEntry: false}, 0o700, "hex"][index];
            }
            for (let i = 0; i < 4; i++) {
                try {
                    STAT(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);

            // Supported options:
            const s1 = STAT("/sandbox/f1.txt");
            const s2 = STAT("/sandbox/f1.txt", undefined);
            const s3 = STAT("/sandbox/f1.txt", { throwIfNoEntry: true });
            const s4 = STAT("/sandbox/f1.txt", { bigint: false });
            console.log(s1.size + "," + s2.size + "," + s3.size + "," + s4.size);
        "#
        .replace("IMPORT", import)
        .replace("STAT", stat);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        let stdout = support::stdout(&output);
        assert!(stdout.contains("path;options;path;options;path;options;path;options;"), "{import}");
        assert!(stdout.contains("5,5,5,5"), "{import}");
    }
    let output = support::run(
        r#"
            import { statSync } from "fs";
            try { statSync("/no-preopen/private", { bigint: true }); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: statSync options are not supported\n"
    );
}

#[test]
fn unlink_options_are_evaluated_and_rejected_before_deletion() {
    for (import, unlink) in [
        ("import * as fs from 'fs';", "fs.unlinkSync"),
        ("import { unlinkSync } from 'node:fs';", "unlinkSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        let target = directory.join("survivor.txt");
        fs::write(&target, "keep me").unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox/survivor.txt";
            }
            function options(index: number) {
                evaluations += "options;";
                return [{}, {recursive: true}, 0o700, "extra"][index];
            }
            for (let i = 0; i < 4; i++) {
                try {
                    UNLINK(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);
            UNLINK("/sandbox/survivor.txt", undefined);
        "#
        .replace("IMPORT", import)
        .replace("UNLINK", unlink);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        let stdout = support::stdout(&output);
        assert!(stdout.contains("path;options;path;options;path;options;path;options;"), "{import}");
        assert!(!target.exists(), "{import}");
    }
    let output = support::run(
        r#"
            import { unlinkSync } from "fs";
            try { unlinkSync("/no-preopen/private", {}); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: unlinkSync does not accept options\n"
    );
}

#[test]
fn rmdir_options_are_evaluated_and_rejected_before_removal() {
    for (import, rmdir) in [
        ("import * as fs from 'fs';", "fs.rmdirSync"),
        ("import { rmdirSync } from 'node:fs';", "rmdirSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        let target = directory.join("target_dir");
        fs::create_dir(&target).unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox/target_dir";
            }
            function options(index: number) {
                evaluations += "options;";
                return [{recursive: true}, {maxRetries: 3}, 0o700, "extra"][index];
            }
            for (let i = 0; i < 4; i++) {
                try {
                    RMDIR(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);
            RMDIR("/sandbox/target_dir", undefined);
        "#
        .replace("IMPORT", import)
        .replace("RMDIR", rmdir);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        let stdout = support::stdout(&output);
        assert!(stdout.contains("path;options;path;options;path;options;path;options;"), "{import}");
        assert!(!target.exists(), "{import}");
    }
    let output = support::run(
        r#"
            import { rmdirSync } from "fs";
            try { rmdirSync("/no-preopen/private", { recursive: true }); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: rmdirSync options are not supported\n"
    );
}

#[test]
fn read_options_are_evaluated_and_rejected_before_file_opening() {
    for (import, read) in [
        ("import * as fs from 'fs';", "fs.readFileSync"),
        ("import { readFileSync } from 'node:fs';", "readFileSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("test.txt"), "hello world").unwrap();
        let source = r#"
            IMPORT
            let evaluations = "";
            function path(index: number) {
                evaluations += "path;";
                return "/sandbox/test.txt";
            }
            function options(index: number) {
                evaluations += "options;";
                return ["hex", "base64", "ascii", {encoding: "hex"}, {flag: "r+"}, {invalid: true}, 42][index];
            }
            for (let i = 0; i < 7; i++) {
                try {
                    READ(path(i), options(i));
                    console.log("unreachable");
                } catch (error) { console.log(error); }
            }
            console.log(evaluations);

            // Verify supported options
            const bin1 = READ("/sandbox/test.txt");
            const bin2 = READ("/sandbox/test.txt", undefined);
            const bin3 = READ("/sandbox/test.txt", null);
            const bin4 = READ("/sandbox/test.txt", "binary");
            const bin5 = READ("/sandbox/test.txt", { encoding: "binary" });
            const bin6 = READ("/sandbox/test.txt", { flag: "r" });
            console.log(bin1.length + "," + bin2.length + "," + bin3.length + "," + bin4.length + "," + bin5.length + "," + bin6.length);

            const str1 = READ("/sandbox/test.txt", "utf8");
            const str2 = READ("/sandbox/test.txt", "utf-8");
            const str3 = READ("/sandbox/test.txt", { encoding: "utf8" });
            const str4 = READ("/sandbox/test.txt", { encoding: "utf-8", flag: "r" });
            console.log(str1 + "," + str2 + "," + str3 + "," + str4);
        "#
        .replace("IMPORT", import)
        .replace("READ", read);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        let stdout = support::stdout(&output);
        assert!(stdout.contains("path;options;path;options;path;options;path;options;path;options;path;options;path;options;"), "{import}");
        assert!(stdout.contains("11,11,11,11,11,11"), "{import}");
        assert!(stdout.contains("hello world,hello world,hello world,hello world"), "{import}");
    }
    // Rejection occurs before checking preopens:
    let output = support::run(
        r#"
            import { readFileSync } from "fs";
            try { readFileSync("/no-preopen/private.txt", "hex"); console.log("unreachable"); }
            catch (error) { console.log(error); }
        "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeError: Unsupported readFileSync options; only UTF-8 and binary encodings are supported\n"
    );
}

#[test]
fn binary_write_encoding_requires_byte_views_and_preserves_files_on_rejection() {
    for (import, write) in [
        ("import * as fs from 'fs';", "fs.writeFileSync"),
        ("import { writeFileSync } from 'node:fs';", "writeFileSync"),
    ] {
        let scratch = support::Scratch::new();
        let directory = scratch.0.join("sandbox");
        fs::create_dir(&directory).unwrap();
        fs::write(directory.join("existing.txt"), "original").unwrap();
        let source = r#"
            IMPORT
            const options = ["binary", {encoding: "binary"}];
            const bytes = Uint8Array.from([90, 233, 0, 255, 91]).subarray(1, 4);
            for (let i = 0; i < options.length; i++) {
                try {
                    WRITE("/sandbox/existing.txt", "é", options[i]);
                    console.log("unreachable");
                } catch (error) { console.log("caught"); }
                try {
                    WRITE("/sandbox/missing.txt", "é", options[i]);
                    console.log("unreachable");
                } catch (error) { console.log("caught"); }
                WRITE("/sandbox/bytes" + i, bytes, options[i]);
            }
            WRITE("/sandbox/utf8.txt", "é", "utf8");
        "#
        .replace("IMPORT", import)
        .replace("WRITE", write);
        let wasm = scratch.compile(&source, None);
        let output = Command::new(support::get_wasmtime_path())
            .args(["run", "-C", "cache=n", "--dir"])
            .arg(format!("{}::/sandbox", directory.display()))
            .arg(wasm)
            .output()
            .unwrap();
        assert_eq!(support::stdout(&output), "caught\n".repeat(4), "{import}");
        assert_eq!(
            fs::read(directory.join("existing.txt")).unwrap(),
            b"original"
        );
        assert!(!directory.join("missing.txt").exists());
        for index in 0..2 {
            assert_eq!(
                fs::read(directory.join(format!("bytes{index}"))).unwrap(),
                [233, 0, 255]
            );
        }
        assert_eq!(fs::read(directory.join("utf8.txt")).unwrap(), [0xc3, 0xa9]);
    }
}

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

#[test]
fn fs_binary_read_write_roundtrip() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        // Non-UTF8 byte sequence: 0x00, 0xFF, 0xFE, 0x80, 0x42
        const buf = new Uint8Array(5);
        buf[0] = 0;
        buf[1] = 255;
        buf[2] = 254;
        buf[3] = 128;
        buf[4] = 66;

        fs.writeFileSync("/sandbox/binary.dat", buf);

        const readBack = fs.readFileSync("/sandbox/binary.dat");
        console.log("BIN_LEN=" + readBack.length);
        console.log("BIN_0=" + readBack[0]);
        console.log("BIN_1=" + readBack[1]);
        console.log("BIN_2=" + readBack[2]);
        console.log("BIN_3=" + readBack[3]);
        console.log("BIN_4=" + readBack[4]);

        const sub = readBack.subarray(1, 4);
        console.log("SUB_LEN=" + sub.length);
        console.log("SUB_0=" + sub[0]);
        console.log("SUB_2=" + sub[2]);
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

    assert!(stdout.contains("BIN_LEN=5"), "stdout: {stdout}");
    assert!(stdout.contains("BIN_0=0"), "stdout: {stdout}");
    assert!(stdout.contains("BIN_1=255"), "stdout: {stdout}");
    assert!(stdout.contains("BIN_2=254"), "stdout: {stdout}");
    assert!(stdout.contains("BIN_3=128"), "stdout: {stdout}");
    assert!(stdout.contains("BIN_4=66"), "stdout: {stdout}");
    assert!(stdout.contains("SUB_LEN=3"), "stdout: {stdout}");
    assert!(stdout.contains("SUB_0=255"), "stdout: {stdout}");
    assert!(stdout.contains("SUB_2=128"), "stdout: {stdout}");
}

#[test]
fn fs_binary_subview_write_and_empty() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        // Write from a subarray with non-zero byte_offset
        const base = new Uint8Array(8);
        for (let i = 0; i < 8; i++) {
            base[i] = i * 10;
        }
        const slice = base.subarray(2, 6); // [20, 30, 40, 50]
        fs.writeFileSync("/sandbox/slice.dat", slice);

        const readSlice = fs.readFileSync("/sandbox/slice.dat");
        console.log("SLICE_LEN=" + readSlice.length);
        console.log("SLICE_0=" + readSlice[0]);
        console.log("SLICE_3=" + readSlice[3]);

        // Empty Uint8Array write
        const empty = new Uint8Array(0);
        fs.writeFileSync("/sandbox/empty.dat", empty);
        const readEmpty = fs.readFileSync("/sandbox/empty.dat");
        console.log("EMPTY_LEN=" + readEmpty.length);
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

    assert!(stdout.contains("SLICE_LEN=4"), "stdout: {stdout}");
    assert!(stdout.contains("SLICE_0=20"), "stdout: {stdout}");
    assert!(stdout.contains("SLICE_3=50"), "stdout: {stdout}");
    assert!(stdout.contains("EMPTY_LEN=0"), "stdout: {stdout}");
}

#[test]
fn fs_binary_repeated_operations() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        for (let i = 0; i < 200; i++) {
            const buf = new Uint8Array(4);
            buf[0] = i & 0xff;
            buf[1] = (i + 1) & 0xff;
            buf[2] = (i + 2) & 0xff;
            buf[3] = (i + 3) & 0xff;

            const path = "/sandbox/bin_" + (i % 5) + ".dat";
            fs.writeFileSync(path, buf);
            const read = fs.readFileSync(path);
            if (read.length !== 4 || read[0] !== (i & 0xff)) {
                throw new Error("Mismatch at iteration " + i);
            }
        }
        console.log("BINARY_CYCLES_PASSED=200");
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
        stdout.contains("BINARY_CYCLES_PASSED=200"),
        "stdout: {stdout}"
    );
}

#[test]
fn fs_exists_and_mkdir_and_readdir() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        console.log("EXISTS_BEFORE=" + fs.existsSync("/sandbox/folder"));
        fs.mkdirSync("/sandbox/folder");
        console.log("EXISTS_AFTER=" + fs.existsSync("/sandbox/folder"));

        fs.writeFileSync("/sandbox/folder/file1.txt", "hello 1");
        fs.writeFileSync("/sandbox/folder/file2.txt", "hello 2");

        const entries = fs.readdirSync("/sandbox/folder");
        console.log("ENTRIES_LEN=" + entries.length);
        console.log("HAS_FILE1=" + entries.includes("file1.txt"));
        console.log("HAS_FILE2=" + entries.includes("file2.txt"));
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

    assert!(stdout.contains("EXISTS_BEFORE=false"), "stdout: {stdout}");
    assert!(stdout.contains("EXISTS_AFTER=true"), "stdout: {stdout}");
    assert!(stdout.contains("ENTRIES_LEN=2"), "stdout: {stdout}");
    assert!(stdout.contains("HAS_FILE1=true"), "stdout: {stdout}");
    assert!(stdout.contains("HAS_FILE2=true"), "stdout: {stdout}");
}

#[test]
fn fs_stat_file_and_directory() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        fs.writeFileSync("/sandbox/test_stat.txt", "1234567890");
        const fileStat = fs.statSync("/sandbox/test_stat.txt");
        console.log("FILE_IS_FILE=" + fileStat.isFile());
        console.log("FILE_IS_DIR=" + fileStat.isDirectory());
        console.log("FILE_SIZE=" + fileStat.size);
        console.log("FILE_HAS_MTIME=" + (fileStat.mtimeMs > 0));

        const dirStat = fs.statSync("/sandbox");
        console.log("DIR_IS_FILE=" + dirStat.isFile());
        console.log("DIR_IS_DIR=" + dirStat.isDirectory());
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

    assert!(stdout.contains("FILE_IS_FILE=true"), "stdout: {stdout}");
    assert!(stdout.contains("FILE_IS_DIR=false"), "stdout: {stdout}");
    assert!(stdout.contains("FILE_SIZE=10"), "stdout: {stdout}");
    assert!(stdout.contains("FILE_HAS_MTIME=true"), "stdout: {stdout}");
    assert!(stdout.contains("DIR_IS_FILE=false"), "stdout: {stdout}");
    assert!(stdout.contains("DIR_IS_DIR=true"), "stdout: {stdout}");
}

#[test]
fn fs_unlink_and_rmdir() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        fs.mkdirSync("/sandbox/temp_dir");
        fs.writeFileSync("/sandbox/temp_dir/temp_file.txt", "temp");

        console.log("FILE_EXISTS_PRE=" + fs.existsSync("/sandbox/temp_dir/temp_file.txt"));
        fs.unlinkSync("/sandbox/temp_dir/temp_file.txt");
        console.log("FILE_EXISTS_POST=" + fs.existsSync("/sandbox/temp_dir/temp_file.txt"));

        console.log("DIR_EXISTS_PRE=" + fs.existsSync("/sandbox/temp_dir"));
        fs.rmdirSync("/sandbox/temp_dir");
        console.log("DIR_EXISTS_POST=" + fs.existsSync("/sandbox/temp_dir"));
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

    assert!(stdout.contains("FILE_EXISTS_PRE=true"), "stdout: {stdout}");
    assert!(
        stdout.contains("FILE_EXISTS_POST=false"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("DIR_EXISTS_PRE=true"), "stdout: {stdout}");
    assert!(stdout.contains("DIR_EXISTS_POST=false"), "stdout: {stdout}");
}

#[test]
fn fs_metadata_error_handling() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import * as fs from "fs";

        fs.writeFileSync("/sandbox/plain.txt", "not a directory");

        let enotdirCaught = false;
        try {
            fs.readdirSync("/sandbox/plain.txt");
        } catch (e: any) {
            const msg = "" + e;
            console.log("CAUGHT_READDIR=" + msg);
            enotdirCaught = msg.includes("ENOTDIR");
        }
        console.log("ENOTDIR_PASSED=" + enotdirCaught);

        let enoentStatCaught = false;
        try {
            fs.statSync("/sandbox/does_not_exist.txt");
        } catch (e: any) {
            const msg = "" + e;
            console.log("CAUGHT_STAT=" + msg);
            enoentStatCaught = msg.includes("ENOENT");
        }
        console.log("ENOENT_STAT_PASSED=" + enoentStatCaught);

        let enoentUnlinkCaught = false;
        try {
            fs.unlinkSync("/sandbox/does_not_exist.txt");
        } catch (e: any) {
            const msg = "" + e;
            console.log("CAUGHT_UNLINK=" + msg);
            enoentUnlinkCaught = msg.includes("ENOENT");
        }
        console.log("ENOENT_UNLINK_PASSED=" + enoentUnlinkCaught);
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

    assert!(stdout.contains("ENOTDIR_PASSED=true"), "stdout: {stdout}");
    assert!(
        stdout.contains("ENOENT_STAT_PASSED=true"),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("ENOENT_UNLINK_PASSED=true"),
        "stdout: {stdout}"
    );
}

#[test]
fn fs_named_imports_metadata_execution() {
    let scratch = support::Scratch::new();
    let host_dir = scratch.0.join("sandbox");
    fs::create_dir_all(&host_dir).unwrap();

    let wasm = scratch.compile(
        r#"
        import { existsSync, mkdirSync, readdirSync, statSync, unlinkSync } from "node:fs";

        mkdirSync("/sandbox/named_dir");
        console.log("NAMED_EXISTS=" + existsSync("/sandbox/named_dir"));

        const entries = readdirSync("/sandbox");
        console.log("NAMED_HAS_DIR=" + entries.includes("named_dir"));

        const st = statSync("/sandbox/named_dir");
        console.log("NAMED_IS_DIR=" + st.isDirectory());
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

    assert!(stdout.contains("NAMED_EXISTS=true"), "stdout: {stdout}");
    assert!(stdout.contains("NAMED_HAS_DIR=true"), "stdout: {stdout}");
    assert!(stdout.contains("NAMED_IS_DIR=true"), "stdout: {stdout}");
}
