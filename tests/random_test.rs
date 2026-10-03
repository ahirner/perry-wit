//! Tests for Phase 7: WASI Preview 2 Randomness (`wasi:random`).

mod support;

use std::{fs, process::Command};

#[test]
fn random_validation_precedes_host_calls_and_mutation() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export fill: func(size: f64) -> f64;
            export fill-tail: func() -> f64;
            export reject: func(index: f64) -> f64;
        }}
    "#,
        include_str!("../wit/world.wit")
    );
    let compiled = scratch.compile_artifacts(
        r#"
        export function fill(size: number): number {
            const bytes = new Uint8Array(size);
            bytes[0] = 7;
            bytes[size - 1] = 9;
            try {
                const returned = crypto.getRandomValues(bytes);
                if (returned !== bytes) { return -1; }
                return size === 0 ? 0 : bytes[0];
            } catch { return bytes[0] + bytes[size - 1]; }
        }
        export function reject(index: number): number {
            const values = [[1, 2, 3], null, undefined, "bytes", 42, {}, true];
            const input = values[index];
            try { crypto.getRandomValues(input); return -1; }
            catch { return 1; }
        }
        export function fillTail(): number {
            const bytes = new Uint8Array(65538);
            bytes[0] = 7;
            const tail = bytes.subarray(-2);
            crypto.getRandomValues(tail);
            return bytes[0] + bytes[65537];
        }
    "#,
        Some(&wit),
    );
    let path = scratch.0.join("random.wasm");
    fs::write(&path, compiled.core).unwrap();
    let output = Command::new("node")
        .arg("--eval")
        .arg(
            r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        const imports = {};
        const requests = [];
        let rt;
        for (const {module: mod, name} of WebAssembly.Module.imports(module)) {
            let implementation = () => { throw new Error(`unexpected import ${mod} ${name}`); };
            if (mod === 'wasi:random/random@0.2.6' && name === 'get-random-bytes') {
                implementation = (length, result) => {
                    requests.push(Number(length));
                    assert.ok(length <= 65536n);
                    const ptr = rt.cabi_realloc(0, 0, 1, Number(length));
                    new Uint8Array(rt.memory.buffer, ptr, Number(length)).fill(165);
                    const memory = new DataView(rt.memory.buffer);
                    memory.setUint32(result, ptr, true);
                    memory.setUint32(result + 4, Number(length), true);
                };
            }
            (imports[mod] ??= {})[name] = implementation;
        }
        rt = new WebAssembly.Instance(module, imports).exports;
        for (let index = 0; index < 7; index++) { assert.equal(rt.reject(index), 1); }
        assert.equal(rt.fill(65537), 16);
        assert.deepEqual(requests, []);
        assert.equal(rt.fill(65536), 165);
        assert.equal(rt.fill(0), 0);
        assert.equal(rt['fill-tail'](), 172);
        assert.deepEqual(requests, [65536, 2]);
    "#,
        )
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = support::run(
        r#"
        try {
            crypto.getRandomValues(new Uint8Array(65537));
            console.log("unreachable");
        } catch (error) { console.log(error); }
    "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "QuotaExceededError: Random view exceeds 65536 bytes\n"
    );
}

#[test]
fn random_rejects_ordinary_arrays_and_null_without_mutating_them() {
    let output = support::run(
        r#"
        const array = [1, 2, 3];
        try { crypto.getRandomValues(array); console.log("unreachable"); }
        catch (error) { console.log(error); }
        console.log(JSON.stringify(array));
        try { crypto.getRandomValues(null); console.log("unreachable"); }
        catch (error) { console.log(error); }
    "#,
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "TypeMismatchError: Expected an integer typed-array view\n[1,2,3]\nTypeMismatchError: Expected an integer typed-array view\n"
    );
}

#[test]
fn math_random_returns_values_in_unit_interval() {
    let source = r#"
        for (let i = 0; i < 5; i++) {
            const r = Math.random();
            console.log(r >= 0.0 && r < 1.0 ? "valid_range" : "invalid");
        }
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "valid_range\nvalid_range\nvalid_range\nvalid_range\nvalid_range\n"
    );
}

#[test]
fn crypto_random_uuid_formats_valid_rfc4122_v4() {
    let source = r#"
        const id1 = crypto.randomUUID();
        const id2 = crypto.randomUUID();
        console.log("len=" + id1.length);
        console.log("hyphen1=" + (id1.charAt(8) === '-'));
        console.log("hyphen2=" + (id1.charAt(13) === '-'));
        console.log("hyphen3=" + (id1.charAt(18) === '-'));
        console.log("hyphen4=" + (id1.charAt(23) === '-'));
        console.log("v4=" + (id1.charAt(14) === '4'));
        const var_char = id1.charAt(19);
        console.log("variant=" + (var_char === '8' || var_char === '9' || var_char === 'a' || var_char === 'b'));
        console.log("diff=" + (id1 !== id2));
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "len=36\nhyphen1=true\nhyphen2=true\nhyphen3=true\nhyphen4=true\nv4=true\nvariant=true\ndiff=true\n"
    );
}

#[test]
fn crypto_get_random_values_mutates_uint8array_in_place_and_returns_array() {
    let source = r#"
        const arr = new Uint8Array(16);
        const ret = crypto.getRandomValues(arr);
        console.log("same_ref=" + (arr === ret));
        console.log("len=" + arr.length);
        let has_nonzero = false;
        for (let i = 0; i < arr.length; i++) {
            if (arr[i] > 0) has_nonzero = true;
        }
        console.log("has_nonzero=" + has_nonzero);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "same_ref=true\nlen=16\nhhas_nonzero=true\n".replace("hhas_nonzero", "has_nonzero")
    );
}

#[test]
fn crypto_get_random_values_on_subarray() {
    let source = r#"
        const orig = new Uint8Array(8);
        orig[0] = 77;
        orig[1] = 88;
        const sub = orig.subarray(-6);
        crypto.getRandomValues(sub);
        console.log("head0=" + orig[0]);
        console.log("head1=" + orig[1]);
        let sub_nonzero = false;
        for (let i = 0; i < sub.length; i++) {
            if (sub[i] > 0) sub_nonzero = true;
        }
        console.log("sub_nonzero=" + sub_nonzero);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "head0=77\nhead1=88\nsub_nonzero=true\n"
    );
}

#[test]
fn random_component_prunes_http_and_clocks() {
    let source = r#"
        const uuid = crypto.randomUUID();
        console.log("generated=" + (uuid.length === 36));
    "#;
    let output = support::run(source, None, None);
    assert_eq!(support::stdout(&output).trim(), "generated=true");

    let scratch = support::Scratch::new();
    let compiled = scratch.compile_artifacts(source, None);
    let component_wasm = compiled.component.expect("component artifact");
    let wat = wasmprinter::print_bytes(&component_wasm).expect("print component wat");

    assert!(
        wat.contains("wasi:random"),
        "random component should import wasi:random"
    );
    assert!(
        !wat.contains("wasi:http"),
        "random component should not import wasi:http"
    );
    assert!(
        !wat.contains("wasi:clocks"),
        "random component should not import wasi:clocks"
    );
}

#[test]
fn controlled_host_inputs_for_math_random_and_uuid_generation() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export test-random: func() -> f64;
            export test-uuid: func() -> string;
        }}
    "#,
        include_str!("../wit/world.wit")
    );
    let compiled = scratch.compile_artifacts(
        r#"
        export function testRandom(): number {
            return Math.random();
        }
        export function testUuid(): string {
            return crypto.randomUUID();
        }
    "#,
        Some(&wit),
    );
    let path = scratch.0.join("random_edges.wasm");
    fs::write(&path, compiled.core).unwrap();
    let output = Command::new("node")
        .arg("--eval")
        .arg(
            r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        let mockU64 = 0n;
        let mockBytes = [];
        const imports = {};
        let rt;
        for (const {module: mod, name} of WebAssembly.Module.imports(module)) {
            let implementation = () => { throw new Error(`unexpected import ${mod} ${name}`); };
            if (mod === 'wasi:random/insecure@0.2.6' && name === 'get-insecure-random-u64') {
                implementation = () => mockU64;
            } else if (mod === 'wasi:random/random@0.2.6' && name === 'get-random-bytes') {
                implementation = (length, result) => {
                    const len = mockBytes.length;
                    const ptr = rt.cabi_realloc(0, 0, 1, len);
                    new Uint8Array(rt.memory.buffer, ptr, len).set(mockBytes);
                    const memory = new DataView(rt.memory.buffer);
                    memory.setUint32(result, ptr, true);
                    memory.setUint32(result + 4, len, true);
                };
            }
            (imports[mod] ??= {})[name] = implementation;
        }
        rt = new WebAssembly.Instance(module, imports).exports;

        function getUuid() {
            const area = rt['test-uuid']();
            const view = new DataView(rt.memory.buffer);
            const ptr = view.getUint32(area, true);
            const len = view.getUint32(area + 4, true);
            const str = new TextDecoder().decode(new Uint8Array(rt.memory.buffer, ptr, len));
            if (rt.cabi_post_cleanup) { rt.cabi_post_cleanup(); }
            return str;
        }

        // Test 1: Math.random() with controlled edge values
        // Lower bound: 0 -> exactly 0.0
        mockU64 = 0n;
        assert.equal(rt['test-random'](), 0.0);

        // Upper bound: (1 << 53) - 1 -> (2^53 - 1) / 2^53
        mockU64 = (1n << 53n) - 1n;
        const maxVal = rt['test-random']();
        assert.equal(maxVal, ((2**53) - 1) / (2**53));
        assert.ok(maxVal < 1.0);
        assert.ok(maxVal >= 0.0);

        // Full 64-bit max: u64::MAX -> upper 11 bits ignored by mantissa mask
        mockU64 = 0xFFFFFFFFFFFFFFFFn;
        const max64Val = rt['test-random']();
        assert.equal(max64Val, ((2**53) - 1) / (2**53));
        assert.ok(max64Val < 1.0);

        // Mid-point: 1 << 52 -> 0.5
        mockU64 = 1n << 52n;
        assert.equal(rt['test-random'](), 0.5);

        // Only high bits set (> 53 bits): mantissa is 0 -> 0.0
        mockU64 = 0xFFE0000000000000n;
        assert.equal(rt['test-random'](), 0.0);

        // Test 2: crypto.randomUUID() with controlled byte patterns
        // Case A: All zeroes
        mockBytes = new Array(16).fill(0);
        const zeroUuid = getUuid();
        assert.equal(zeroUuid, '00000000-0000-4000-8000-000000000000');
        assert.equal(zeroUuid.length, 36);
        assert.equal(zeroUuid.charAt(14), '4', 'RFC 4122 v4 version digit');
        assert.equal(zeroUuid.charAt(19), '8', 'RFC 4122 variant digit (10xx)');

        // Case B: All 0xFF
        mockBytes = new Array(16).fill(0xff);
        const ffUuid = getUuid();
        assert.equal(ffUuid, 'ffffffff-ffff-4fff-bfff-ffffffffffff');
        assert.equal(ffUuid.charAt(14), '4', 'RFC 4122 v4 version digit');
        assert.equal(ffUuid.charAt(19), 'b', 'RFC 4122 variant digit (1011)');

        // Case C: Exact byte sequence vector
        // [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10]
        // byte 6: 0xcd -> (0xcd & 0x0f) | 0x40 = 0x4d
        // byte 8: 0xfe -> (0xfe & 0x3f) | 0x80 = 0xbe
        mockBytes = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10];
        const vectorUuid = getUuid();
        assert.equal(vectorUuid, '01234567-89ab-4def-bedc-ba9876543210');

        // Case D: Short host response (< 16 bytes, e.g. empty)
        // Guest runtime pads with zeroes up to 16 bytes
        mockBytes = [];
        const shortUuid = getUuid();
        assert.equal(shortUuid, '00000000-0000-4000-8000-000000000000');
    "#,
        )
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
