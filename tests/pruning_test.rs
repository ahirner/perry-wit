//! Verification for Item C: Safe Runtime Pruning (C.1 Separable Capability Dispatch & C.2 Pruned Components).

#[allow(dead_code)]
#[path = "support/http_fixture.rs"]
mod http_fixture;
mod support;

use http_fixture::{HttpFixture, Reply};
use std::{fs, process::Command};

#[test]
fn synchronous_capability_combinations_import_only_requested_interfaces() {
    let capabilities = [
        (
            "wasi:clocks/",
            "console.log(Date.now() > 0 && performance.now() >= 0);",
        ),
        (
            "wasi:random/",
            "console.log(Math.random() >= 0 && crypto.randomUUID().length === 36);",
        ),
        (
            "wasi:cli/environment",
            "console.log(process.env.PERRY_PRUNING === 'env');",
        ),
        (
            "wasi:filesystem/",
            "import * as fs from 'node:fs'; console.log(fs.readFileSync('/sandbox/value.txt', 'utf8') === 'file');",
        ),
    ];
    let scratch = support::Scratch::new();
    fs::write(scratch.0.join("value.txt"), "file").unwrap();
    for mask in 0u32..16 {
        let mut source = String::from("console.log('complete');\n");
        for (index, (_, code)) in capabilities.iter().enumerate() {
            if mask & (1 << index) != 0 {
                source.push_str(code);
                source.push('\n');
            }
        }
        let compiled = scratch.compile_artifacts(&source, None);
        wasmparser::Validator::new()
            .validate_all(&compiled.core)
            .unwrap();
        let mut imports = Vec::new();
        for payload in wasmparser::Parser::new(0).parse_all(&compiled.core) {
            if let wasmparser::Payload::ImportSection(reader) = payload.unwrap() {
                for import in reader.into_imports() {
                    imports.push(import.unwrap().module.to_owned());
                }
            }
        }
        for (index, (interface, _)) in capabilities.iter().enumerate() {
            assert_eq!(
                imports.iter().any(|name| name.starts_with(interface)),
                mask & (1 << index) != 0,
                "capability mask {mask}: unexpected imports for {interface}: {imports:?}"
            );
        }
        assert!(!imports.iter().any(|name| name.starts_with("wasi:http/")));

        let component = scratch.0.join("test.wasm");
        fs::write(&component, compiled.component.unwrap()).unwrap();
        let output = Command::new(support::get_wasmtime_path())
            .args([
                "run",
                "-C",
                "cache=n",
                "--env",
                "PERRY_PRUNING=env",
                "--dir",
            ])
            .arg(format!("{}::/sandbox", scratch.0.display()))
            .arg(component)
            .output()
            .unwrap();
        assert_eq!(
            support::stdout(&output),
            format!("complete\n{}", "true\n".repeat(mask.count_ones() as usize)),
            "capability mask {mask}"
        );
    }
}

#[test]
fn pure_await_and_promise_all_work_without_http_imports() {
    for clock_use in ["", "Date.now();"] {
        let source = format!(
            r#"
            {clock_use}
            console.log(await 42);
            console.log(await "value");
            console.log(await null);
            console.log(await undefined);
            console.log(JSON.stringify(await Promise.all([1, 2])));
            console.log(JSON.stringify(await Promise.all([1, "two", null, true])));
            console.log(JSON.stringify(await Promise.all([])));
        "#
        );
        let output = support::run(&source, None, None);
        assert_eq!(
            support::stdout(&output),
            "42\nvalue\nnull\nundefined\n[1,2]\n[1,\"two\",null,true]\n[]\n"
        );
        let scratch = support::Scratch::new();
        let compiled = scratch.compile_artifacts(&source, None);
        let wat = wasmprinter::print_bytes(compiled.component.unwrap()).unwrap();
        assert!(!wat.contains("wasi:http"));
        assert_eq!(wat.contains("wasi:clocks"), !clock_use.is_empty());
    }
}

#[test]
fn test_pure_typescript_prunes_http_and_clocks() {
    let ts_source = r#"
        const a = 10;
        const b = 20;
        console.log("sum=" + (a + b));
    "#;
    let output = support::run(ts_source, None, None);
    assert_eq!(support::stdout(&output).trim(), "sum=30");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    // Pure component must NOT import wasi:http, wasi:clocks, or wasi:random
    assert!(
        !wat.contains("wasi:http"),
        "pure component should not contain wasi:http imports"
    );
    assert!(
        !wat.contains("wasi:clocks"),
        "pure component should not contain wasi:clocks imports"
    );
    assert!(
        !wat.contains("wasi:random"),
        "pure component should not contain wasi:random imports"
    );
}

#[test]
fn test_clocks_only_component_prunes_http() {
    let ts_source = r#"
        const now = Date.now();
        const p = performance.now();
        console.log("clocks_ok");
    "#;
    let output = support::run(ts_source, None, None);
    assert_eq!(support::stdout(&output).trim(), "clocks_ok");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    // Clocks component must import wasi:clocks, but must NOT import wasi:http or wasi:random
    assert!(
        wat.contains("wasi:clocks"),
        "clocks component should import wasi:clocks"
    );
    assert!(
        !wat.contains("wasi:http"),
        "clocks component should not contain wasi:http imports"
    );
    assert!(
        !wat.contains("wasi:random"),
        "clocks component should not contain wasi:random imports"
    );
}

#[test]
fn test_http_component_retains_http_imports() {
    let ts_source = r#"
        const res = fetch("https://example.com");
        console.log(res.status);
    "#;
    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(ts_source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    assert!(
        wat.contains("wasi:http"),
        "http component should retain wasi:http imports"
    );
}

#[test]
fn exported_json_task_prunes_http_and_runs_without_http_bindings() {
    let ts_source = r#"
        export function runTask(input: string): string {
            const data = JSON.parse(input);
            const output = {
                received: data.items,
                count: data.items.length,
                status: "ok"
            };
            return JSON.stringify(output);
        }
    "#;

    let scratch = support::Scratch::new();
    let options = perry_wit::compiler::CompileOptions {
        out_path: None,
        runtime_path: None,
        wit_dir: std::path::PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        core_only: false,
    };
    let compiled = perry_wit::compiler::compile_typescript(ts_source, "task.ts", &options).unwrap();
    let component_bytes = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_bytes).expect("print component wat");

    // Pure task MUST NOT import wasi:http or any other unused interfaces
    assert!(
        !wat.contains("wasi:http"),
        "pure JSON task should omit wasi:http imports"
    );
    assert!(
        !wat.contains("wasi:clocks"),
        "pure JSON task should omit wasi:clocks imports"
    );
    assert!(
        !wat.contains("wasi:filesystem"),
        "pure JSON task should omit wasi:filesystem imports"
    );
    assert!(
        !wat.contains("wasi:random"),
        "pure JSON task should omit wasi:random imports"
    );

    // Verify component export
    assert!(
        wat.contains("(export \"run-task\" (func"),
        "component must export run-task"
    );

    // Write component to disk and run with wasmtime WITHOUT -S http=y or network flags
    let component_path = scratch.0.join("json_task.wasm");
    fs::write(&component_path, &component_bytes).unwrap();

    let output = Command::new(support::get_wasmtime_path())
        .args([
            "run",
            "-C",
            "cache=n",
            "--invoke",
            r#"run-task("{\"items\":[\"apple\",\"banana\",\"cherry\"]}")"#,
        ])
        .arg(&component_path)
        .output()
        .expect("wasmtime execution");

    assert!(
        output.status.success(),
        "wasmtime failed without HTTP bindings:\nStdout: {}\nStderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("apple")
            && stdout.contains("banana")
            && stdout.contains("cherry")
            && stdout.contains("count")
            && stdout.contains("ok"),
        "unexpected output: {stdout}"
    );
}

#[test]
fn http_task_retains_http_and_verifies_abi_marshalling_and_cleanup() {
    let fixture = HttpFixture::new(|request| {
        Reply::Body(
            200,
            format!("response-for:{}", String::from_utf8_lossy(&request.body)),
        )
    });
    let wit = r#"package test:http-task;
        world test {
            import wasi:cli/stdout@0.2.6;
            import wasi:cli/stderr@0.2.6;
            import wasi:cli/exit@0.2.6;
            import wasi:cli/environment@0.2.6;
            import wasi:clocks/wall-clock@0.2.6;
            import wasi:clocks/monotonic-clock@0.2.6;
            import wasi:http/outgoing-handler@0.2.6;
            import wasi:http/types@0.2.6;
            import wasi:io/poll@0.2.6;
            import wasi:io/streams@0.2.6;
            import wasi:random/random@0.2.6;
            import wasi:random/insecure@0.2.6;
            import wasi:filesystem/types@0.2.6;
            import wasi:filesystem/preopens@0.2.6;
            export run-task: func(input: string) -> result<string, string>;
        }
    "#;
    let ts_source = format!(
        r#"
        export function runTask(input: string): any {{
            const res = fetch("http://{}/", {{
                method: "POST",
                body: input
            }});
            return {{ ok: true, value: "got:" + res.status }};
        }}
        "#,
        fixture.address
    );

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(&ts_source, Some(wit));
    let component_bytes = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_bytes).expect("print component wat");

    // Retained HTTP imports
    assert!(
        wat.contains("wasi:http"),
        "HTTP task must retain wasi:http imports"
    );

    // Verify component export signature in WIT/metadata
    assert!(
        wat.contains("(export \"run-task\" (func"),
        "component must export run-task"
    );

    // Verify core module exports cabi_realloc and cabi_post_run-task
    let mut core_exports = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&compiled.core) {
        if let wasmparser::Payload::ExportSection(reader) = payload.unwrap() {
            for exp in reader {
                core_exports.push(exp.unwrap().name.to_string());
            }
        }
    }
    assert!(
        core_exports.contains(&"cabi_realloc".to_string()),
        "core must export cabi_realloc"
    );
    assert!(
        core_exports.contains(&"cabi_post_run-task".to_string()),
        "core must export cabi_post_run-task"
    );

    // Run component through wasmtime with HTTP enabled
    let component_path = scratch.0.join("http_task.wasm");
    fs::write(&component_path, &component_bytes).unwrap();

    let output = Command::new(support::get_wasmtime_path())
        .args([
            "run",
            "-C",
            "cache=n",
            "-S",
            "http=y",
            "-S",
            "inherit-network=y",
            "--invoke",
            r#"run-task("hello-perry")"#,
        ])
        .arg(&component_path)
        .output()
        .expect("wasmtime execution");

    assert!(
        output.status.success(),
        "wasmtime failed:\nStdout: {}\nStderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok(\"got:200\")") || stdout.contains("got:200"),
        "unexpected output: {stdout}"
    );

    // Verify the HTTP fixture received the request body
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        1,
        "fixture should receive exactly 1 request"
    );
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].body, b"hello-perry");
}

#[test]
fn record_pruning_and_component_sizes() {
    let options = perry_wit::compiler::CompileOptions {
        out_path: None,
        runtime_path: None,
        wit_dir: std::path::PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        core_only: false,
    };

    // 1. Pure JSON Task
    let pure_ts = r#"
        export function runTask(input: string): string {
            const data = JSON.parse(input);
            return JSON.stringify({ items: data.items, count: data.items.length });
        }
    "#;
    let pure_compiled =
        perry_wit::compiler::compile_typescript(pure_ts, "pure_task.ts", &options).unwrap();

    // 2. HTTP Task
    let http_ts = r#"
        export function runTask(input: string): string {
            const res = fetch("https://example.com/api", { method: "POST", body: input });
            return "status=" + res.status;
        }
    "#;
    let http_compiled =
        perry_wit::compiler::compile_typescript(http_ts, "http_task.ts", &options).unwrap();

    let rt_bytes = perry_wit::runtime::resolve_guest_runtime_bytes(None).expect("resolve runtime");

    let pure_raw = pure_compiled.raw_core.len();
    let pure_unlinked = pure_raw + rt_bytes.len();
    let pure_core = pure_compiled.core.len();
    let pure_stripped = pure_compiled.stripped.as_ref().unwrap().len();

    let http_raw = http_compiled.raw_core.len();
    let http_unlinked = http_raw + rt_bytes.len();
    let http_core = http_compiled.core.len();
    let http_stripped = http_compiled.stripped.as_ref().unwrap().len();

    println!("=== Pruning Size Comparison ===");
    println!("Pure JSON Task:");
    println!("  Raw TS Wasm:            {pure_raw} bytes");
    println!("  Guest Runtime Wasm:     {} bytes", rt_bytes.len());
    println!("  Unlinked Total:         {pure_unlinked} bytes");
    println!(
        "  Pruned Merged Core:     {pure_core} bytes ({:.1}% of unlinked)",
        (pure_core as f64 / pure_unlinked as f64) * 100.0
    );
    println!("  Stripped Component:     {pure_stripped} bytes");
    println!("HTTP Task:");
    println!("  Raw TS Wasm:            {http_raw} bytes");
    println!("  Guest Runtime Wasm:     {} bytes", rt_bytes.len());
    println!("  Unlinked Total:         {http_unlinked} bytes");
    println!(
        "  Pruned Merged Core:     {http_core} bytes ({:.1}% of unlinked)",
        (http_core as f64 / http_unlinked as f64) * 100.0
    );
    println!("  Stripped Component:     {http_stripped} bytes");

    // Pure core must be pruned and significantly smaller than unlinked
    assert!(pure_core < pure_unlinked);
    assert!(http_core < http_unlinked);
    // Pure task prunes HTTP runtime logic so its core must be smaller than HTTP task
    assert!(
        pure_core < http_core,
        "pure core ({pure_core}) must be smaller than http core ({http_core})"
    );
}
