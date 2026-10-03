//! Integration tests for repeated task invocations, value lifetimes, and bounded memory (Item E.1).

#[expect(
    dead_code,
    reason = "This suite shares scratch storage and stdout checks, but has its own host runners."
)]
mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use perry_wit::compiler::{CompileOptions, compile_typescript};

#[test]
fn scheduled_callbacks_release_captures_resources_and_stale_ids() {
    let scratch = support::Scratch::new();
    let artifacts = scratch.compile_artifacts(
        r#"
        let trace = "";
        let previousId = 0;
        let intervalCount = 0;
        let latest = "";
        let previousInterval = 0;
        export function runTask(input: string): string {
            trace = "";
            const owner: any = {text: input, bytes: Uint8Array.from([17, 128, 255])};
            const ids = {later: 0};
            const record = suffix => {
                trace += owner.text + suffix + owner.bytes[1] + ";";
                if (input === "component") { console.log(trace); }
            };
            owner.callback = record;
            owner.self = owner;
            setTimeout(() => {
                record(":first:");
                clearTimeout(ids.later);
                setTimeout(() => record(":nested:"), 1);
            }, 10);
            const canceled = setTimeout(() => record(":canceled:"), 5);
            clearTimeout(canceled);
            clearTimeout(canceled);
            ids.later = setTimeout(() => record(":later:"), 20);
            const live = setTimeout(record, 10, ":second:");
            clearTimeout(live + 0.5);
            clearTimeout(-live);
            clearTimeout(Infinity);
            clearTimeout(previousId);
            previousId = live;
            setTimeout(() => {
                try { throw owner.text; }
                catch (error) { record(":caught:"); }
            }, 30);
            if (input.startsWith("fail:")) {
                setTimeout(() => { throw owner.text; }, 1);
            }
            if (input.startsWith("sync-fail:")) { throw owner.text; }
            return "scheduled";
        }
        export function snapshot(): string { return trace; }
        export function idle(): string {
            const id = setTimeout(() => { trace = "unreachable"; }, 1000);
            clearTimeout(id);
            return "idle";
        }
        export function delayCases(): string {
            const values: any[] = [undefined, null, NaN, Infinity, -1, 0, 1.9, "2.9", 2147483648];
            for (let index = 0; index < values.length; index++) {
                const id = setTimeout(() => { trace = "unreachable"; }, values[index]);
                clearTimeout(id);
            }
            return "checked";
        }
        export function intervalTask(input: string, ticks: number): string {
            intervalCount = 0;
            latest = "";
            const owner: any = {text: input, bytes: Uint8Array.from([17, 128]), id: 0};
            const callback = bytes => {
                intervalCount++;
                bytes[0] = intervalCount % 256;
                const temporary: any = {
                    text: JSON.stringify({input: owner.text, count: intervalCount}),
                    bytes: new Uint8Array(2048)
                };
                temporary.self = temporary;
                if (temporary.bytes.length !== 2048 || bytes !== owner.bytes) { throw "lost view"; }
                try { throw temporary.text; }
                catch (error) {
                    if (!error.startsWith("{\"input\":")) { throw "lost temporary"; }
                }
                latest = owner.text + ":" + intervalCount + ":" + owner.bytes[0];
                if (input === "component") { console.log(latest); }
                if (input.startsWith("interval-fail:") && intervalCount === 2) { throw owner.text; }
                if (intervalCount === ticks) { clearTimeout(owner.id); }
            };
            owner.callback = callback;
            owner.self = owner;
            owner.id = setInterval(callback, 2, owner.bytes);
            clearInterval(previousInterval);
            previousInterval = owner.id;
            if (input.startsWith("interval-fail:")) {
                setTimeout(() => { latest = "unreachable"; }, 1000);
            }
            return "returned:" + input;
        }
        export function intervalSnapshot(): string { return latest; }
        export function intervalResult(input: string, ticks: number): any {
            intervalTask(input, ticks);
            return {ok: true, value: "result:" + input};
        }
        export function intervalVoid(ticks: number): void {
            intervalTask("void", ticks);
        }
    "#,
        Some(
            r#"
        package test:timers;
        world test {
            import wasi:cli/stdout@0.2.6;
            import wasi:cli/stderr@0.2.6;
            import wasi:cli/exit@0.2.6;
            import wasi:io/poll@0.2.6;
            import wasi:io/streams@0.2.6;
            import wasi:clocks/monotonic-clock@0.2.6;
            export run-task: func(input: string) -> string;
            export snapshot: func() -> string;
            export idle: func() -> string;
            export delay-cases: func() -> string;
            export interval-task: func(input: string, ticks: u32) -> string;
            export interval-snapshot: func() -> string;
            export interval-result: func(input: string, ticks: u32) -> result<string, string>;
            export interval-void: func(ticks: u32);
        }
    "#,
        ),
    );
    let core = scratch.0.join("core.wasm");
    fs::write(&core, artifacts.core).unwrap();
    let init_core = scratch.0.join("init-failure.wasm");
    let init_artifacts =
        scratch.compile_artifacts("setTimeout(() => {}, 1000); throw 'initialization';", None);
    fs::write(&init_core, init_artifacts.core).unwrap();
    let wat = wasmprinter::print_bytes(artifacts.component.as_ref().unwrap()).unwrap();
    for capability in [
        "wasi:http",
        "wasi:filesystem",
        "wasi:random",
        "wasi:clocks/wall-clock",
        "wasi:cli/environment",
    ] {
        assert!(!wat.contains(capability), "timer task retains {capability}");
    }
    let output = Command::new("node").arg("--eval").arg(r#"
        const assert = require('node:assert/strict');
        const module_ = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        const imports = {};
        let now = 0n, nextPollable = 0, blocked = 0, created = 0, dropped = 0, peak = 0;
        let late = false, memoryLimit;
        let deliveryBudget = 10;
        const pending = new Map();
        const delays = [];
        for (const {module, name} of WebAssembly.Module.imports(module_)) {
            let implementation = () => { throw Error(`unexpected host call ${module}.${name}`); };
            if (module.startsWith('wasi:clocks/monotonic-clock')) {
                if (name === 'now') { implementation = () => now; }
                if (name === 'subscribe-instant') {
                    implementation = deadline => {
                        const id = ++nextPollable;
                        pending.set(id, deadline);
                        delays.push(Number(deadline - now));
                        peak = Math.max(peak, pending.size);
                        created++;
                        return id;
                    };
                }
            } else if (module.startsWith('wasi:io/poll')) {
                if (name === '[method]pollable.block') {
                    implementation = id => {
                        assert.ok(pending.has(id));
                        assert.ok(deliveryBudget-- > 0, 'timer delivery must terminate within its expected callback count');
                        if (pending.get(id) > now) { now = pending.get(id); }
                        if (late) { now += 5_000_000n; }
                        if (memoryLimit !== undefined) {
                            assert.equal(e.memory.buffer.byteLength, memoryLimit, 'memory must stay bounded within a long-running interval');
                        }
                        blocked++;
                    };
                } else if (name === '[resource-drop]pollable') {
                    implementation = id => { assert.ok(pending.delete(id)); dropped++; };
                }
            } else if (module.startsWith('wasi:cli/stderr')) {
                implementation = () => 1;
            } else if (module.startsWith('wasi:io/streams')) {
                if (name === '[method]output-stream.blocking-write-and-flush') {
                    implementation = (id, ptr, len, ret) => { new Uint8Array(e.memory.buffer)[ret] = 0; };
                } else if (name === '[resource-drop]output-stream') { implementation = () => {}; }
            } else if (module.startsWith('wasi:cli/exit')) { implementation = () => {}; }
            (imports[module] ??= {})[name] = implementation;
        }
        let e = new WebAssembly.Instance(module_, imports).exports;
        function call(name, input, post = true, ticks) {
            deliveryBudget = ticks === undefined ? 10 : ticks + 2;
            const bytes = input === undefined ? null : new TextEncoder().encode(input);
            const ptr = bytes ? e.cabi_realloc(0, 0, 1, bytes.length) : 0;
            if (bytes) { new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes); }
            try {
                const ret = bytes ? (ticks === undefined ? e[name](ptr, bytes.length) : e[name](ptr, bytes.length, ticks)) : e[name]();
                const isResult = name === 'interval-result';
                const words = new Uint32Array(e.memory.buffer, ret, isResult ? 3 : 2);
                if (isResult) { assert.equal(words[0], 0); }
                const offset = isResult ? 1 : 0;
                const result = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[offset], words[offset + 1]));
                if (post) { e[`cabi_post_${name}`](ret); }
                return result;
            } finally { if (bytes) { e.cabi_realloc(ptr, bytes.length, 1, 0); } }
        }
        function cycle(index) {
            const text = `cycle-${String(index).padStart(5, '0')}:` + 'retained😀'.repeat(40);
            const start = now;
            const blocks = blocked;
            assert.equal(call('run-task', text, index % 7 !== 0), 'scheduled');
            assert.equal(now - start, 30_000_000n);
            assert.equal(blocked - blocks, 4);
            assert.equal(call('snapshot'), ['first', 'second', 'nested', 'caught'].map(suffix => text + ':' + suffix + ':128;').join(''));
            assert.equal(pending.size, 0);
            const idleBlocks = blocked;
            assert.equal(call('idle'), 'idle');
            assert.equal(blocked, idleBlocks, 'a cancelled queue must terminate without polling');
            assert.equal(pending.size, 0);
            assert.throws(() => call('run-task', 'fail:' + text), WebAssembly.RuntimeError);
            assert.equal(pending.size, 0, 'uncaught callback failure must cancel every subscription');
            assert.equal(call('snapshot'), '', 'a failed callback must prevent later callbacks');
            const beforeSyncFailure = blocked;
            assert.throws(() => call('run-task', 'sync-fail:' + text), WebAssembly.RuntimeError);
            assert.equal(pending.size, 0, 'a synchronous guest failure must cancel pending timers');
            assert.equal(blocked, beforeSyncFailure);
        }
        assert.equal(call('delay-cases'), 'checked');
        assert.deepEqual(delays, [1, 1, 1, 1, 1, 1, 1, 2, 1].map(value => value * 1_000_000));
        delays.length = 0;
        for (let index = 0; index < 100; index++) { cycle(index); }
        const memory = e.memory.buffer.byteLength;
        for (let index = 0; index < 1000; index++) { cycle(index); }
        assert.equal(e.memory.buffer.byteLength, memory, 'timer/capture cycles must stop growing after warm-up');

        const text = 'interval😀'.repeat(100);
        assert.equal(call('interval-task', text, true, 100), 'returned:' + text);
        memoryLimit = e.memory.buffer.byteLength;
        for (let index = 0; index < 10; index++) {
            const start = now, blocks = blocked;
            assert.equal(call('interval-task', text, index % 3 !== 0, 5000), 'returned:' + text);
            assert.equal(blocked - blocks, 5000);
            assert.equal(now - start, 10_000_000_000n);
            assert.equal(call('interval-snapshot'), text + ':5000:136');
            assert.equal(pending.size, 0);
        }
        assert.equal(call('interval-result', text, true, 3), 'result:' + text);
        deliveryBudget = 3;
        e['interval-void'](3);
        assert.equal(call('interval-snapshot'), 'void:3:3');
        delays.length = 0;
        late = true;
        const start = now;
        assert.equal(call('interval-task', text, true, 3), 'returned:' + text);
        assert.equal(now - start, 21_000_000n);
        assert.deepEqual(delays, [2_000_000, 2_000_000, 2_000_000], 'late intervals rearm from callback start without missed-tick bursts');
        late = false;
        for (let index = 0; index < 100; index++) {
            const failed = 'interval-fail:' + text;
            assert.throws(() => call('interval-task', failed, true, 3), WebAssembly.RuntimeError);
            assert.equal(pending.size, 0, 'interval failure releases the active interval and other subscriptions');
            assert.equal(call('interval-snapshot'), failed + ':2:2');
            assert.equal(call('interval-task', text, true, 3), 'returned:' + text);
        }
        assert.equal(e.memory.buffer.byteLength, memoryLimit);
        assert.equal(created, dropped);
        assert.ok(peak <= 5, `pollables must remain bounded, observed ${peak}`);
        const initModule = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[2]));
        e = new WebAssembly.Instance(initModule, imports).exports;
        const beforeInitialization = blocked;
        assert.throws(() => e['wasi:cli/run@0.2.6#run'](), WebAssembly.RuntimeError);
        assert.equal(pending.size, 0, 'initialization failure must release queued subscriptions');
        assert.equal(blocked, beforeInitialization);
        assert.equal(created, dropped);
    "#).arg(&core).arg(&init_core).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let component = scratch.0.join("component.wasm");
    fs::write(&component, artifacts.component.unwrap()).unwrap();
    let output = get_wasmtime_cmd()
        .args([
            "run",
            "-C",
            "cache=n",
            "--invoke",
            "run-task(\"component\")",
        ])
        .arg(component)
        .output()
        .unwrap();
    assert_eq!(
        support::stdout(&output),
        "component:first:128;\ncomponent:first:128;component:second:128;\ncomponent:first:128;component:second:128;component:nested:128;\ncomponent:first:128;component:second:128;component:nested:128;component:caught:128;\n\"scheduled\"\n"
    );
    let output = get_wasmtime_cmd()
        .args([
            "run",
            "-C",
            "cache=n",
            "--invoke",
            "interval-task(\"component\",3)",
        ])
        .arg(scratch.0.join("component.wasm"))
        .output()
        .unwrap();
    assert_eq!(
        support::stdout(&output),
        "component:1:1\ncomponent:2:2\ncomponent:3:3\n\"returned:component\"\n"
    );
}

#[test]
fn retained_callback_graphs_survive_calls_and_release_on_success_and_failure() {
    let scratch = support::Scratch::new();
    let source = r#"
        let callback: any = null;
        export function retain(input: string): string {
            const owner: any = {text: input, bytes: Uint8Array.from([17, 128, 255])};
            const next = (suffix: string): string => {
                if (suffix === "fail") { throw owner.text; }
                return owner.text + ":" + suffix + ":" + owner.bytes[1];
            };
            owner.callback = next;
            owner.self = owner;
            callback = next;
            return "retained";
        }
        export function invoke(input: string): any {
            if (callback === null) { return {ok: false, error: "empty"}; }
            try {
                const current = callback;
                return {ok: true, value: current(input)};
            } catch (error) { return {ok: false, error}; }
        }
        export function release(): string {
            callback = null;
            return "released";
        }
        export function scenario(input: string): string {
            retain(input);
            const current = callback;
            const value = current("component");
            release();
            return value;
        }
    "#;
    let artifacts = scratch.compile_artifacts(
        source,
        Some(
            r#"
        package test:callbacks;
        world test {
            import wasi:cli/stdout@0.2.6;
            import wasi:cli/stderr@0.2.6;
            import wasi:cli/exit@0.2.6;
            import wasi:io/poll@0.2.6;
            import wasi:io/streams@0.2.6;
            export retain: func(value: string) -> string;
            export invoke: func(value: string) -> result<string, string>;
            export release: func() -> string;
            export scenario: func(value: string) -> string;
        }
    "#,
        ),
    );
    let core = scratch.0.join("core.wasm");
    fs::write(&core, artifacts.core).unwrap();
    let output = Command::new("node").arg("--eval").arg(r#"
        const assert = require('node:assert/strict');
        const bytes = require('node:fs').readFileSync(process.argv[1]);
        const module_ = new WebAssembly.Module(bytes);
        const imports = {};
        for (const {module, name} of WebAssembly.Module.imports(module_)) {
            (imports[module] ??= {})[name] = () => { throw Error(`unexpected host call ${module}.${name}`); };
        }
        const e = new WebAssembly.Instance(module_, imports).exports;
        function call(name, input, post = true) {
            const bytes = input === undefined ? null : new TextEncoder().encode(input);
            const ptr = bytes ? e.cabi_realloc(0, 0, 1, bytes.length) : 0;
            if (bytes) { new Uint8Array(e.memory.buffer, ptr, bytes.length).set(bytes); }
            const ret = bytes ? e[name](ptr, bytes.length) : e[name]();
            if (bytes) { e.cabi_realloc(ptr, bytes.length, 1, 0); }
            const result = name === 'invoke';
            const words = new Uint32Array(e.memory.buffer, ret, result ? 3 : 2);
            const offset = result ? 1 : 0;
            const value = new TextDecoder().decode(new Uint8Array(e.memory.buffer, words[offset], words[offset + 1]));
            const error = result && words[0] === 1;
            if (post) { e[`cabi_post_${name}`](ret); }
            return result ? {error, value} : value;
        }
        function cycle(index) {
            const captured = `capture-${index}:` + 'retained😀'.repeat(100);
            assert.equal(call('retain', captured, index % 7 !== 0), 'retained');
            for (let invocation = 0; invocation < 4; invocation++) {
                assert.deepEqual(call('invoke', 'success', invocation % 3 !== 0), {error: false, value: captured + ':success:128'});
            }
            assert.deepEqual(call('invoke', 'fail'), {error: true, value: captured});
            assert.deepEqual(call('invoke', 'recovered'), {error: false, value: captured + ':recovered:128'});
            assert.equal(call('release'), 'released');
            assert.deepEqual(call('invoke', 'after-release'), {error: true, value: 'empty'});
        }
        for (let index = 0; index < 100; index++) { cycle(index); }
        const memory = e.memory.buffer.byteLength;
        for (let index = 0; index < 1000; index++) { cycle(index); }
        assert.equal(e.memory.buffer.byteLength, memory, 'Callback/capture cycles must stop growing after warm-up');
    "#).arg(&core).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let component = scratch.0.join("component.wasm");
    fs::write(&component, artifacts.component.unwrap()).unwrap();
    let output = get_wasmtime_cmd()
        .args(["run", "-C", "cache=n", "--invoke", "scenario(\"guest\")"])
        .arg(component)
        .output()
        .unwrap();
    assert_eq!(support::stdout(&output).trim(), "\"guest:component:128\"");
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
