//! Tests for Item D.1: Shared Binary Values & Uint8Array Views.

mod support;

use std::{fs, process::Command};

#[test]
fn constructor_lengths_coerce_and_invalid_lengths_throw_at_the_call() {
    assert_matches_node(
        r#"
        const sizes = [undefined, null, false, true, NaN, -0.5, 2.9, "3"];
        for (let i = 0; i < sizes.length; i++) {
            const bytes = new Uint8Array(sizes[i]);
            console.log(bytes.length);
        }
        function construct(size) {
            const bytes = new Uint8Array(size);
            console.log("unreachable");
        }
        const invalid = [-1, -1.5, Infinity, -Infinity, "-1", "1e999"];
        for (let i = 0; i < invalid.length; i++) {
            try { construct(invalid[i]); console.log("unreachable caller"); }
            catch { console.log("caught"); }
            finally { console.log("cleanup"); }
        }
    "#,
    );
    assert_direct_runtime(
        r#"
        for (const invalid of [-1, -1.5, Infinity, -Infinity, 2 ** 32]) {
            assert.equal(rt.uint8array_new(value(invalid)), 0x7ffc000000000001n);
            assert.equal(rt.has_exception(), 1);
            rt.get_exception();
        }
        for (const size of [NaN, -0.5, 2.9]) {
            const view = rt.uint8array_new(value(size));
            assert.equal(number(rt.uint8array_length(view)), new Uint8Array(size).length);
            assert.equal(rt.has_exception(), 0);
        }
    "#,
    );
    let output = support::run(
        "new Uint8Array(-1); console.log('unreachable');",
        None,
        None,
    );
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("RangeError"));
}

#[test]
fn subarray_bounds_are_relative_to_the_current_view() {
    assert_matches_node(
        r#"
        const bytes = Uint8Array.from([10, 20, 30, 40]);
        const bounds = [[-2, undefined], [0, -1], [-99, 99], [3, 1],
                        [-2.9, -0.5], [NaN, Infinity], [-Infinity, 2]];
        for (let i = 0; i < bounds.length; i++) {
            console.log(JSON.stringify(bytes.subarray(bounds[i][0], bounds[i][1])));
        }
        const middle = bytes.subarray(1, -1);
        const tail = middle.subarray(-1);
        tail[0] = 77;
        console.log(JSON.stringify(bytes));
        console.log(JSON.stringify(middle));
    "#,
    );
    assert_direct_runtime(
        r#"
        const source = rt.uint8array_from(importJson([10, 20, 30, 40]));
        const middle = rt.buffer_slice(source, value(1), value(-1));
        const tail = rt.buffer_slice(middle, value(-1), 0x7ffc000000000001n);
        rt.uint8array_set(tail, value(0), value(77));
        assert.equal(number(rt.uint8array_length(middle)), 2);
        assert.equal(number(rt.uint8array_length(tail)), 1);
        assert.equal(number(rt.uint8array_get(source, value(2))), 77);
        assert.equal(number(rt.uint8array_get(source, value(3))), 40);
    "#,
    );
}

#[test]
fn invalid_element_indices_never_alias_bytes() {
    assert_matches_node(
        r#"
        function read(view, key) { return view[key]; }
        function write(view, key, value) { view[key] = value; }
        const bytes = Uint8Array.from([10, 20]);
        const invalid = [-1, -0.5, 1.5, NaN, Infinity, -Infinity, 2, 4294967296];
        for (let i = 0; i < invalid.length; i++) {
            const key = invalid[i];
            console.log(bytes[key]);
            bytes[key] = 99;
            console.log(read(bytes, key));
            write(bytes, key, 88);
        }
        console.log(JSON.stringify(bytes));
        console.log(read(bytes, "0"));
        console.log(read(bytes, "1"));
        console.log(read(bytes, "-0"));
        console.log(read(bytes, "01"));
        console.log(bytes[-0]);
    "#,
    );
    assert_direct_runtime(
        r#"
        const bytes = rt.uint8array_new(importJson([10, 20]));
        for (const key of [-1, -0.5, 1.5, NaN, Infinity, -Infinity, 2, 4294967296]) {
            assert.equal(rt.uint8array_get(bytes, value(key)), 0x7ffc000000000001n);
            rt.uint8array_set(bytes, value(key), value(99));
            rt.object_set_dynamic(bytes, value(key), value(88));
        }
        assert.equal(number(rt.uint8array_get(bytes, value(0))), 10);
        assert.equal(number(rt.uint8array_get(bytes, value(1))), 20);
    "#,
    );
}

#[test]
fn constructors_copy_arrays_and_visible_view_bytes_independently() {
    assert_matches_node(
        r#"
        const unrelated = [{a: 1}, [2, 3], new Uint8Array(9)];
        const array = [10, 20, 257, -1];
        const source = new Uint8Array(array);
        const copy = new Uint8Array(source.subarray(1, 3));
        array[0] = 99;
        source[1] = 88;
        copy[1] = 77;
        console.log(JSON.stringify(source));
        console.log(JSON.stringify(copy));
        const empty = new Uint8Array([]);
        const numeric = new Uint8Array("3");
        console.log(empty.length);
        console.log(numeric.length);
    "#,
    );
    assert_direct_runtime(
        r#"
        const source = rt.uint8array_new(importJson([10, 20, 257, -1]));
        const copy = rt.uint8array_new(source);
        rt.uint8array_set(source, value(0), value(99));
        assert.equal(number(rt.uint8array_length(copy)), 4);
        assert.equal(number(rt.uint8array_get(copy, value(0))), 10);
        assert.equal(number(rt.uint8array_get(copy, value(2))), 1);
    "#,
    );
}

#[test]
fn byte_construction_and_writes_coerce_truncate_and_wrap() {
    assert_matches_node(
        r#"
        const values = [256, 257, -1, -257, 258.9, -258.9, NaN, Infinity, -Infinity,
                        true, false, null, undefined, "257", "-1", "0x100", ""];
        const from = Uint8Array.from(values);
        const assigned = new Uint8Array(values.length);
        for (let i = 0; i < values.length; i++) { assigned[i] = values[i]; }
        console.log(JSON.stringify(from));
        console.log(JSON.stringify(assigned));
    "#,
    );
    assert_direct_runtime(
        r#"
        const values = [256, 257, -1, -257, 258.9, -258.9];
        const view = rt.uint8array_from(importJson(values));
        for (let i = 0; i < values.length; i++) {
            assert.equal(number(rt.uint8array_get(view, value(i))), new Uint8Array(values)[i]);
            rt.uint8array_set(view, value(i), value(values[values.length - 1 - i]));
            assert.equal(number(rt.uint8array_get(view, value(i))), new Uint8Array(values.toReversed())[i]);
        }
    "#,
    );
}

#[test]
fn typed_array_json_uses_numeric_object_keys_including_nested_views() {
    assert_matches_node(
        r#"
        const bytes = Uint8Array.from([7, 8, 9]);
        console.log(JSON.stringify(bytes));
        console.log(JSON.stringify({ bytes: bytes, nested: [bytes.subarray(1)] }));
        console.log(JSON.stringify(new Uint8Array(0)));
    "#,
    );
}

fn assert_matches_node(source: &str) {
    let expected = Command::new("node")
        .args(["--eval", source])
        .output()
        .unwrap();
    let actual = support::run(source, None, None);
    assert_eq!(support::stdout(&actual), support::stdout(&expected));
}

fn assert_direct_runtime(script: &str) {
    let scratch = support::Scratch::new();
    let path = scratch.0.join("runtime.wasm");
    fs::write(
        &path,
        perry_wit::runtime::resolve_guest_runtime_bytes(None).unwrap(),
    )
    .unwrap();
    let script = [r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        const memory = new WebAssembly.Memory({initial: 32});
        const imports = {env: {memory}};
        for (const {module: mod, name, kind} of WebAssembly.Module.imports(module)) {
            if (kind === 'function') {
                (imports[mod] ??= {})[name] = () => { throw new Error(`unexpected import ${mod} ${name}`); };
            }
        }
        const rt = new WebAssembly.Instance(module, imports).exports;
        const bits = new DataView(new ArrayBuffer(8));
        function value(n) { bits.setFloat64(0, n, true); return bits.getBigInt64(0, true); }
        function number(n) { bits.setBigInt64(0, n, true); return bits.getFloat64(0, true); }
        function importJson(input) {
            const bytes = new TextEncoder().encode(JSON.stringify(input));
            const ptr = rt.cabi_realloc(0, 0, 1, bytes.length);
            new Uint8Array(memory.buffer, ptr, bytes.length).set(bytes);
            return rt.cabi_import_json(ptr, bytes.length);
        }
    "#, script].concat();
    let output = Command::new("node")
        .args(["--eval", &script])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn uint8array_basic_construction_and_indexed_access() {
    let source = r#"
        const buf = new Uint8Array(4);
        console.log("len:" + buf.length);
        console.log("b0:" + buf[0]);
        console.log("b3:" + buf[3]);

        buf[0] = 12;
        buf[1] = 34;
        buf[2] = 56;
        buf[3] = 78;

        console.log("after0:" + buf[0]);
        console.log("after1:" + buf[1]);
        console.log("after2:" + buf[2]);
        console.log("after3:" + buf[3]);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "len:4\nb0:0\nb3:0\nafter0:12\nafter1:34\nafter2:56\nafter3:78\n"
    );
}

#[test]
fn uint8array_from_elements() {
    let source = r#"
        const arr = Uint8Array.from([10, 20, 30, 40, 50]);
        console.log("len:" + arr.length);
        console.log("arr[0]:" + arr[0]);
        console.log("arr[4]:" + arr[4]);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(support::stdout(&output), "len:5\narr[0]:10\narr[4]:50\n");
}

#[test]
fn uint8array_bounds_and_empty_views() {
    let source = r#"
        const empty = new Uint8Array(0);
        console.log("empty len:" + empty.length);
        console.log("empty[0]:" + empty[0]);

        const buf = new Uint8Array(2);
        buf[0] = 5;
        buf[1] = 10;

        // Out of bounds read returns undefined
        console.log("buf[2]:" + buf[2]);
        console.log("buf[100]:" + buf[100]);

        // Out of bounds write is safely ignored
        buf[2] = 99;
        buf[100] = 100;
        console.log("len unchanged:" + buf.length);
        console.log("buf[0]:" + buf[0]);
        console.log("buf[1]:" + buf[1]);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "empty len:0\nempty[0]:undefined\nbuf[2]:undefined\nbuf[100]:undefined\nlen unchanged:2\nbuf[0]:5\nbuf[1]:10\n"
    );
}

#[test]
fn uint8array_shared_subviews_and_mutation_visibility() {
    let source = r#"
        const orig = Uint8Array.from([1, 2, 3, 4, 5, 6]);
        const sub = orig.subarray(2, 5); // elements at indices 2, 3, 4 -> [3, 4, 5]

        console.log("sub len:" + sub.length);
        console.log("sub[0]:" + sub[0]);
        console.log("sub[1]:" + sub[1]);
        console.log("sub[2]:" + sub[2]);

        // Mutating subview alters underlying buffer and is visible in original
        sub[0] = 99;
        console.log("orig[2] after sub write:" + orig[2]);

        // Mutating original alters underlying buffer and is visible in subview
        orig[3] = 88;
        console.log("sub[1] after orig write:" + sub[1]);

        // Chained overlapping subview
        const sub2 = sub.subarray(1, 3); // relative to sub -> elements [88, 5]
        console.log("sub2 len:" + sub2.length);
        console.log("sub2[0]:" + sub2[0]);
        sub2[0] = 77;
        console.log("sub[1] after sub2 write:" + sub[1]);
        console.log("orig[3] after sub2 write:" + orig[3]);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "sub len:3\nsub[0]:3\nsub[1]:4\nsub[2]:5\norig[2] after sub write:99\nsub[1] after orig write:88\nsub2 len:2\nsub2[0]:88\nsub[1] after sub2 write:77\norig[3] after sub2 write:77\n"
    );
}

#[test]
fn non_utf8_and_binary_edge_values() {
    let source = r#"
        const raw = Uint8Array.from([0, 255, 128, 127, 1]);
        console.log("0:" + raw[0]);
        console.log("255:" + raw[1]);
        console.log("128:" + raw[2]);
        console.log("127:" + raw[3]);
        console.log("1:" + raw[4]);

        const slice = raw.subarray(1, 3);
        console.log("slice[0]:" + slice[0]);
        console.log("slice[1]:" + slice[1]);
    "#;
    let output = support::run(source, None, None);
    assert_eq!(
        support::stdout(&output),
        "0:0\n255:255\n128:128\n127:127\n1:1\nslice[0]:255\nslice[1]:128\n"
    );
}

#[test]
fn pure_component_using_uint8array_has_zero_capability_imports() {
    let source = r#"
        const buf = new Uint8Array(4);
        buf[0] = 65;
        buf[1] = 66;
        console.log("buf0:" + buf[0]);
    "#;
    let scratch = support::Scratch::new();
    let wasm_path = scratch.compile(source, None);

    // Verify component runs and succeeds
    let mut cmd = std::process::Command::new(support::get_wasmtime_path());
    cmd.args(["run", "-C", "cache=n"]);
    cmd.arg(&wasm_path);
    let output = cmd.output().expect("wasmtime execution failed");
    assert_eq!(support::stdout(&output), "buf0:65\n");

    // Inspect wasm imports to ensure wasi:http, wasi:clocks, and wasi:random are all pruned!
    let wasm_bytes = std::fs::read(&wasm_path).unwrap();
    let wat = wasmprinter::print_bytes(&wasm_bytes).expect("wasmprinter failed");
    assert!(
        !wat.contains("wasi:http"),
        "pure component should not import wasi:http"
    );
    assert!(
        !wat.contains("wasi:clocks"),
        "pure component should not import wasi:clocks"
    );
    assert!(
        !wat.contains("wasi:random"),
        "pure component should not import wasi:random"
    );
}
