//! Tests for Phase 7: WASI Preview 2 Randomness (`wasi:random`).

mod support;

use std::{fs, process::Command};

#[test]
fn random_quota_is_checked_before_host_calls_or_mutation() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export fill: func(size: f64) -> f64;
            export fill-tail: func() -> f64;
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
