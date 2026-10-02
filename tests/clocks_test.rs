//! Tests for Phase 6: WASI Preview 2 Clocks and JS Date API.

mod support;

use std::{fs, process::Command};

#[test]
fn invalid_dates_throw_at_the_call_site_and_can_be_caught() {
    let source = r#"
        try {
            const invalid = new Date(8640000000000001);
            console.log(invalid.toISOString());
            console.log("unreachable");
        } catch (error) {
            console.log(error);
        }
        console.log("after");
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "RangeError: Invalid time value\nafter\n"
    );
}

#[test]
fn date_exceptions_unwind_functions_loops_and_finally_blocks() {
    let output = support::run(
        r#"
        function invalid(): string {
            return "prefix:" + new Date(8640000000000001).toISOString();
        }
        function cleanup(): void { console.log("cleanup"); }
        for (let i = 0; i < 2; i++) {
            try {
                try {
                    console.log(invalid());
                    console.log("unreachable");
                } finally {
                    cleanup();
                    console.log("finally end");
                }
                console.log("unreachable after finally");
            } catch (error) {
                console.log(error);
            }
        }
        try {
            try { invalid(); } catch (error) { throw error; }
            finally { console.log("rethrow cleanup"); }
        } catch (error) { console.log(error); }
        try {
            try { invalid(); } finally { throw "replacement"; }
        } catch (error) { console.log(error); }
        for (let i = 0; i < 3; i++) {
            try {
                if (i === 0) { continue; }
                if (i === 2) { break; }
                invalid();
            } catch { console.log("caught without binding"); }
        }
        console.log(new Date(0).toISOString());
    "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "cleanup\nfinally end\nRangeError: Invalid time value\ncleanup\nfinally end\nRangeError: Invalid time value\nrethrow cleanup\nRangeError: Invalid time value\nreplacement\ncaught without binding\n1970-01-01T00:00:00.000Z\n"
    );
}

#[test]
fn uncaught_date_exceptions_fail_command_and_task_invocations() {
    let output = support::run(
        "new Date(8640000000000001).toISOString(); console.log('unreachable');",
        None,
        None,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: RangeError: Invalid time value\n"
    );
    let output = support::run(
        "export function runTask(input: string): string { return new Date(8640000000000001).toISOString(); }",
        Some(&format!(
            "{}\nworld test {{ include runtime-adapter; export run-task: func(input: string) -> string; }}",
            include_str!("../wit/world.wit")
        )),
        Some("run-task(\"invalid\")"),
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "Error: RangeError: Invalid time value\n"
    );
}

#[test]
fn date_constructor_distinguishes_omitted_and_explicit_primitive_arguments() {
    let output = support::run(
        r#"
        function time(value: any): number {
            const date = new Date(value);
            return date.getTime();
        }
        console.log(time(null));
        console.log(time(undefined));
        console.log(time(false));
        console.log(time(true));
        console.log(time(1.9));
        console.log(time(-1.9));
        console.log(new Date().getTime() > 1700000000000);
    "#,
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "0\nNaN\n0\n1\n1\n-1\ntrue\n");
}

#[test]
fn wall_clock_uses_whole_milliseconds_without_reducing_monotonic_precision() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export wall-time: func() -> f64;
            export date-time: func() -> f64;
            export monotonic-time: func() -> f64;
        }}
    "#,
        include_str!("../wit/world.wit")
    );
    let compiled = scratch.compile_artifacts(
        r#"
        export function wallTime(): number { return Date.now(); }
        export function dateTime(): number { return new Date().getTime(); }
        export function monotonicTime(): number { return performance.now(); }
    "#,
        Some(&wit),
    );
    let path = scratch.0.join("clock.wasm");
    fs::write(&path, compiled.core).unwrap();
    let output = Command::new("node").arg("--eval").arg(r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        const imports = {};
        let exports;
        let nanoseconds;
        for (const {module: name, name: operation} of WebAssembly.Module.imports(module)) {
            let implementation = () => { throw new Error(`unexpected import ${name} ${operation}`); };
            if (name === 'wasi:clocks/wall-clock@0.2.6' && operation === 'now') {
                implementation = ptr => {
                    const memory = new DataView(exports.memory.buffer);
                    memory.setBigUint64(ptr, 1700000000n, true);
                    memory.setUint32(ptr + 8, nanoseconds, true);
                };
            } else if (name === 'wasi:clocks/monotonic-clock@0.2.6' && operation === 'now') {
                implementation = () => 123456789n;
            }
            (imports[name] ??= {})[operation] = implementation;
        }
        exports = new WebAssembly.Instance(module, imports).exports;
        for (nanoseconds of [0, 1, 999999, 1000000, 999999999]) {
            const expected = 1700000000000 + Math.floor(nanoseconds / 1000000);
            assert.equal(exports['wall-time'](), expected);
            assert.equal(exports['date-time'](), expected);
        }
        assert.equal(exports['monotonic-time'](), 123.456789);
    "#).arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_date_now_returns_valid_timestamp() {
    let output = support::run(
        r#"
        const now = Date.now();
        console.log(now > 1700000000000 ? "VALID_DATE_NOW" : "INVALID_DATE_NOW");
    "#,
        None,
        None,
    );
    assert_eq!(support::stdout(&output).trim(), "VALID_DATE_NOW");
}

#[test]
fn test_new_date_and_get_time() {
    let output = support::run(
        r#"
        const d = new Date();
        const t = d.getTime();
        console.log(t > 1700000000000 ? "VALID_TIME" : "INVALID_TIME");
    "#,
        None,
        None,
    );
    assert_eq!(support::stdout(&output).trim(), "VALID_TIME");
}

#[test]
fn test_date_calendar_components_and_iso_string() {
    // 1711929600000 ms is 2024-04-01T00:00:00.000Z (Monday)
    let output = support::run(
        r#"
        const d = new Date(1711929600000);
        console.log(d.toISOString());
        console.log("year=" + d.getFullYear());
        console.log("month=" + d.getMonth());
        console.log("date=" + d.getDate());
        console.log("day=" + d.getDay());
        console.log("hours=" + d.getHours());
        console.log("minutes=" + d.getMinutes());
        console.log("seconds=" + d.getSeconds());
    "#,
        None,
        None,
    );
    let out = support::stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "2024-04-01T00:00:00.000Z");
    assert_eq!(lines[1], "year=2024");
    assert_eq!(lines[2], "month=3"); // April is month 3 (0-indexed)
    assert_eq!(lines[3], "date=1");
    assert_eq!(lines[4], "day=1"); // Monday is day 1
    assert_eq!(lines[5], "hours=0");
    assert_eq!(lines[6], "minutes=0");
    assert_eq!(lines[7], "seconds=0");
}

#[test]
fn test_performance_now_monotonicity() {
    let output = support::run(
        r#"
        const t1 = performance.now();
        let sum = 0;
        for (let i = 0; i < 1000; i++) {
            sum += i;
        }
        const t2 = performance.now();
        console.log(t1 >= 0 ? "T1_NON_NEGATIVE" : "T1_NEGATIVE");
        console.log(t2 >= t1 ? "MONOTONIC" : "NON_MONOTONIC");
    "#,
        None,
        None,
    );
    let out = support::stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "T1_NON_NEGATIVE");
    assert_eq!(lines[1], "MONOTONIC");
}
