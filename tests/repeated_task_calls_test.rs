//! Integration tests for repeated task invocations, value lifetimes, and bounded memory (Item E.1).

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use perry_wit::compiler::{CompileOptions, compile_typescript};

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
fn test_repeated_allocating_calls_bounded_memory() {
    let scratch = std::env::temp_dir().join(format!("perry-repeated-mem-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).unwrap();

    let ts_source = r#"
export function runTask(input: string): string {
    let acc = "prefix:";
    for (let i = 0; i < 20; i++) {
        acc = acc + input + ":" + i + ";";
    }
    return acc;
}
"#;

    let options = CompileOptions {
        core_only: true,
        wit_dir: PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        ..Default::default()
    };

    let compiled = compile_typescript(ts_source, "repeated_task.ts", &options)
        .expect("compiling repeated_task.ts");
    fs::write(scratch.join("core.wasm"), &compiled.core).unwrap();

    let check_script = r#"
const fs = require('node:fs');
const assert = require('node:assert/strict');

const wasmBytes = fs.readFileSync(process.argv[2]);
const module_ = new WebAssembly.Module(wasmBytes);
const imports = {};
for (const { module, name } of WebAssembly.Module.imports(module_)) {
    (imports[module] ??= {})[name] = () => { throw new Error(`unexpected import call ${module}.${name}`); };
}

const instance = new WebAssembly.Instance(module_, imports);
const e = instance.exports;

function invokeRunTask(payload) {
    const bytes = new TextEncoder().encode(payload);
    const ptr = e.cabi_realloc(0, 0, 1, bytes.length);
    new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
    const retPtr = e['run-task'](ptr, bytes.length);
    e.cabi_realloc(ptr, bytes.length, 1, 0);
    const words = new Uint32Array(e.memory.buffer, retPtr, 2);
    const resultStr = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[0], words[1]));
    e['cabi_post_run-task'](retPtr);
    return resultStr;
}

// 1. Warm-up
for (let i = 0; i < 100; i++) {
    const res = invokeRunTask("warmup_" + i);
    assert.ok(res.startsWith("prefix:warmup_"));
}

const memoryAfterWarmup = e.memory.buffer.byteLength;

// 2. Execute 5,000 repeated allocating calls
for (let i = 0; i < 5000; i++) {
    const payload = "iteration_" + (i % 10);
    const res = invokeRunTask(payload);
    assert.ok(res.startsWith("prefix:" + payload));
}

// 3. Assert bounded memory: memory high water mark must remain completely stable
const memoryAfter5000 = e.memory.buffer.byteLength;
assert.equal(
    memoryAfter5000,
    memoryAfterWarmup,
    `Linear memory grew from ${memoryAfterWarmup} to ${memoryAfter5000}; temporaries are leaking across calls!`
);
"#;

    fs::write(scratch.join("check.cjs"), check_script).unwrap();
    let output = Command::new("node")
        .arg(scratch.join("check.cjs"))
        .arg(scratch.join("core.wasm"))
        .output()
        .expect("running node check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Node runner failed:\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn test_retained_globals_survive_repeated_calls_and_temporaries_reclaimed() {
    let scratch =
        std::env::temp_dir().join(format!("perry-retained-globals-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).unwrap();

    let ts_source = r#"
let counter = 0;
let store: string[] = [];
let retained: any = null;

export function runTask(input: string): string {
    return "ok:" + input;
}

export function pushItem(item: string): string {
    counter++;
    store.push(item + "_" + counter);
    const bytes = Uint8Array.from([10, 255, 128, 20]);
    const view = bytes.subarray(1, 3);
    retained = { child: { text: item }, bytes: { view }, alias: view };
    retained.self = retained;
    let temp = "temporary_string_to_drop";
    for (let i = 0; i < 15; i++) {
        temp = temp + ":" + i;
    }
    return "count:" + counter;
}

export function getHistory(): string {
    if (retained === null) { return store.join(","); }
    retained.bytes.view[0] = 7;
    return store.join(",") + "|" + retained.child.text + "|" + retained.alias[0]
        + "|" + (retained.alias === retained.bytes.view) + "|" + (retained.self === retained);
}

export function resetHistory(): string {
    store = [];
    retained = null;
    counter = 0;
    return "cleared";
}

export function fallibleTask(input: string): any {
    return { ok: true, value: input };
}
"#;

    let options = CompileOptions {
        core_only: true,
        wit_dir: PathBuf::from("wit"),
        world: Some("repeated-runner".to_string()),
        ..Default::default()
    };

    let compiled = compile_typescript(ts_source, "globals_test.ts", &options)
        .expect("compiling globals_test.ts");
    fs::write(scratch.join("core.wasm"), &compiled.core).unwrap();

    let check_script = r#"
const fs = require('node:fs');
const assert = require('node:assert/strict');

const wasmBytes = fs.readFileSync(process.argv[2]);
const module_ = new WebAssembly.Module(wasmBytes);
const imports = {};
for (const { module, name } of WebAssembly.Module.imports(module_)) {
    (imports[module] ??= {})[name] = () => { throw new Error(`unexpected import call ${module}.${name}`); };
}

const instance = new WebAssembly.Instance(module_, imports);
const e = instance.exports;

function callStringFn(funcName, postFuncName, inputStr) {
    let ptr = 0;
    let len = 0;
    if (inputStr !== undefined) {
        const bytes = new TextEncoder().encode(inputStr);
        ptr = e.cabi_realloc(0, 0, 1, bytes.length);
        new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
        len = bytes.length;
    }
    const retPtr = inputStr !== undefined ? e[funcName](ptr, len) : e[funcName]();
    if (ptr !== 0) {
        e.cabi_realloc(ptr, len, 1, 0);
    }
    const words = new Uint32Array(e.memory.buffer, retPtr, 2);
    const resultStr = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[0], words[1]));
    e[postFuncName](retPtr);
    return resultStr;
}

// 1. Verify retained globals accumulate correctly across distinct invocations
assert.equal(callStringFn('push-item', 'cabi_post_push-item', 'alpha'), 'count:1');
assert.equal(callStringFn('push-item', 'cabi_post_push-item', 'beta'), 'count:2');
assert.equal(callStringFn('push-item', 'cabi_post_push-item', 'gamma'), 'count:3');
assert.equal(callStringFn('get-history', 'cabi_post_get-history'), 'alpha_1,beta_2,gamma_3|gamma|7|true|true');

// 2. Reset globals and verify state is cleared
assert.equal(callStringFn('reset-history', 'cabi_post_reset-history'), 'cleared');
assert.equal(callStringFn('get-history', 'cabi_post_get-history'), '');

// 3. Repeated cycles of pushing and resetting over 2,000 iterations
for (let i = 0; i < 100; i++) {
    callStringFn('push-item', 'cabi_post_push-item', 'warm_' + i);
}
callStringFn('reset-history', 'cabi_post_reset-history');
const baseMemory = e.memory.buffer.byteLength;

for (let cycle = 0; cycle < 1000; cycle++) {
    callStringFn('push-item', 'cabi_post_push-item', 'item_' + cycle);
    const history = callStringFn('get-history', 'cabi_post_get-history');
    assert.ok(history.endsWith('|item_' + cycle + '|7|true|true'), history);
    if (cycle % 10 === 9) {
        callStringFn('reset-history', 'cabi_post_reset-history');
    }
}
callStringFn('reset-history', 'cabi_post_reset-history');
const endMemory = e.memory.buffer.byteLength;

assert.equal(endMemory, baseMemory, 'Memory must remain bounded during repeated global accumulation and resets');
"#;

    fs::write(scratch.join("check.cjs"), check_script).unwrap();
    let output = Command::new("node")
        .arg(scratch.join("check.cjs"))
        .arg(scratch.join("core.wasm"))
        .output()
        .expect("running node check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Node runner failed:\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn test_recoverable_failures_reclaim_temporaries_via_post_return() {
    let scratch =
        std::env::temp_dir().join(format!("perry-recoverable-fail-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).unwrap();

    let ts_source = r#"
export function runTask(input: string): string {
    return "ok:" + input;
}

export function pushItem(item: string): string {
    return item;
}

export function getHistory(): string {
    return "";
}

export function resetHistory(): string {
    return "";
}

export function fallibleTask(input: string): any {
    if (input === "fail") {
        let errDesc = "ERR_DETAIL:";
        for (let i = 0; i < 20; i++) {
            errDesc = errDesc + "err_" + i + ";";
        }
        return { ok: false, error: errDesc };
    }
    let okDesc = "OK_DETAIL:";
    for (let i = 0; i < 20; i++) {
        okDesc = okDesc + "ok_" + i + ";";
    }
    return { ok: true, value: okDesc };
}
"#;

    let options = CompileOptions {
        core_only: true,
        wit_dir: PathBuf::from("wit"),
        world: Some("repeated-runner".to_string()),
        ..Default::default()
    };

    let compiled = compile_typescript(ts_source, "fallible_test.ts", &options)
        .expect("compiling fallible_test.ts");
    fs::write(scratch.join("core.wasm"), &compiled.core).unwrap();

    let check_script = r#"
const fs = require('node:fs');
const assert = require('node:assert/strict');

const wasmBytes = fs.readFileSync(process.argv[2]);
const module_ = new WebAssembly.Module(wasmBytes);
const imports = {};
for (const { module, name } of WebAssembly.Module.imports(module_)) {
    (imports[module] ??= {})[name] = () => { throw new Error(`unexpected import call ${module}.${name}`); };
}

const instance = new WebAssembly.Instance(module_, imports);
const e = instance.exports;

function invokeFallible(input) {
    const bytes = new TextEncoder().encode(input);
    const ptr = e.cabi_realloc(0, 0, 1, bytes.length);
    new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
    const retPtr = e['fallible-task'](ptr, bytes.length);
    e.cabi_realloc(ptr, bytes.length, 1, 0);
    const words = new Uint32Array(e.memory.buffer, retPtr, 3);
    const isError = words[0] === 1;
    const payloadStr = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[1], words[2]));
    e['cabi_post_fallible-task'](retPtr);
    return { isError, payloadStr };
}

// 1. Warm-up
for (let i = 0; i < 100; i++) {
    const res = invokeFallible(i % 2 === 0 ? "fail" : "succeed");
    if (i % 2 === 0) {
        assert.equal(res.isError, true);
        assert.ok(res.payloadStr.startsWith("ERR_DETAIL:"));
    } else {
        assert.equal(res.isError, false);
        assert.ok(res.payloadStr.startsWith("OK_DETAIL:"));
    }
}

const memoryAfterWarmup = e.memory.buffer.byteLength;

// 2. Execute 4,000 repeated fallible calls alternating success and failure
for (let i = 0; i < 4000; i++) {
    const res = invokeFallible(i % 2 === 0 ? "fail" : "succeed");
    if (i % 2 === 0) {
        assert.equal(res.isError, true);
    } else {
        assert.equal(res.isError, false);
    }
}

// 3. Assert memory remained strictly bounded despite error allocations
const memoryAfter4000 = e.memory.buffer.byteLength;
assert.equal(
    memoryAfter4000,
    memoryAfterWarmup,
    `Memory leaked across recoverable failure cycles: ${memoryAfterWarmup} -> ${memoryAfter4000}`
);
"#;

    fs::write(scratch.join("check.cjs"), check_script).unwrap();
    let output = Command::new("node")
        .arg(scratch.join("check.cjs"))
        .arg(scratch.join("core.wasm"))
        .output()
        .expect("running node check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Node runner failed:\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn test_interrupted_cleanup_recovery_and_instance_reusability() {
    let scratch =
        std::env::temp_dir().join(format!("perry-interrupted-clean-{}", std::process::id()));
    let _ = fs::remove_dir_all(&scratch);
    fs::create_dir_all(&scratch).unwrap();

    let ts_source = r#"
let globalCounter = 0;

export function runTask(input: string): string {
    globalCounter++;
    let temporary = "temp_batch:";
    for (let i = 0; i < 25; i++) {
        temporary += input + "_" + i + ";";
    }
    return "result:" + globalCounter + ":" + input;
}
"#;

    let options = CompileOptions {
        core_only: true,
        wit_dir: PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        ..Default::default()
    };

    let compiled = compile_typescript(ts_source, "interrupted_test.ts", &options)
        .expect("compiling interrupted_test.ts");
    fs::write(scratch.join("core.wasm"), &compiled.core).unwrap();

    let check_script = r#"
const fs = require('node:fs');
const assert = require('node:assert/strict');

const wasmBytes = fs.readFileSync(process.argv[2]);
const module_ = new WebAssembly.Module(wasmBytes);
const imports = {};
for (const { module, name } of WebAssembly.Module.imports(module_)) {
    (imports[module] ??= {})[name] = () => { throw new Error(`unexpected import call ${module}.${name}`); };
}

const instance = new WebAssembly.Instance(module_, imports);
const e = instance.exports;

function invokeWithOptionalCleanup(payload, performCleanup) {
    const bytes = new TextEncoder().encode(payload);
    const ptr = e.cabi_realloc(0, 0, 1, bytes.length);
    new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes);
    const retPtr = e['run-task'](ptr, bytes.length);
    e.cabi_realloc(ptr, bytes.length, 1, 0);
    const words = new Uint32Array(e.memory.buffer, retPtr, 2);
    const resultStr = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[0], words[1]));
    if (performCleanup) {
        e['cabi_post_run-task'](retPtr);
    }
    return resultStr;
}

// 1. Normal warm-up
for (let i = 0; i < 50; i++) {
    invokeWithOptionalCleanup("warm_" + i, true);
}
const memoryWarmup = e.memory.buffer.byteLength;

// 2. Interrupted cleanups: host abandons post-return for 500 calls
for (let i = 0; i < 500; i++) {
    const res = invokeWithOptionalCleanup("abandoned_" + i, false);
    assert.ok(res.startsWith("result:"));
}

// 3. Subsequent calls with cleanup
for (let i = 0; i < 500; i++) {
    const res = invokeWithOptionalCleanup("normal_" + i, true);
    assert.ok(res.startsWith("result:"));
}

// 4. Memory must remain bounded after the safe-reset reclaimed abandoned temporaries
const memoryAfter = e.memory.buffer.byteLength;
assert.equal(
    memoryAfter,
    memoryWarmup,
    `Memory leaked after interrupted cleanups: ${memoryWarmup} -> ${memoryAfter}`
);
"#;

    fs::write(scratch.join("check.cjs"), check_script).unwrap();
    let output = Command::new("node")
        .arg(scratch.join("check.cjs"))
        .arg(scratch.join("core.wasm"))
        .output()
        .expect("running node check");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "Node runner failed:\nSTDOUT:\n{stdout}\nSTDERR:\n{stderr}"
    );

    let _ = fs::remove_dir_all(&scratch);
}

#[test]
fn test_wasmtime_cli_repeated_component_invocation() {
    let out_dir = std::env::temp_dir().join("perry_wit_repeated_component_test");
    let _ = fs::create_dir_all(&out_dir);
    let wasm_path = out_dir.join("repeated_task.wasm");

    let ts_source = r#"
export function runTask(input: string): string {
    let s = "COMPUTED:";
    for (let i = 0; i < 10; i++) {
        s = s + input + "-";
    }
    return s;
}
"#;

    let options = CompileOptions {
        out_path: Some(wasm_path.clone()),
        runtime_path: None,
        wit_dir: PathBuf::from("wit"),
        world: Some("task-runner".to_string()),
        core_only: false,
    };

    let compiled =
        compile_typescript(ts_source, "repeated.ts", &options).expect("compiling component");
    let comp_bytes = compiled.component.expect("component bytes");
    fs::write(&wasm_path, &comp_bytes).unwrap();

    let mut cmd = get_wasmtime_cmd();
    let output = cmd
        .args(["run", "--invoke", "run-task(\"echo-payload\")"])
        .arg(&wasm_path)
        .output()
        .expect("Running wasmtime");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "wasmtime invocation failed with status {:?}:\nStderr: {stderr}\nStdout: {stdout}",
        output.status
    );
    assert!(
        stdout.contains("COMPUTED:echo-payload-"),
        "Expected output containing 'COMPUTED:echo-payload-', got: {stdout}"
    );

    let _ = fs::remove_dir_all(&out_dir);
}
