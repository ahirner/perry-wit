#[path = "support/http_fixture.rs"]
mod http_fixture;
mod support;

use http_fixture::{HttpFixture, Reply};
use std::{
    fs,
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn binary_bodies_preserve_subviews_empty_values_and_large_transfers() {
    let fixture = HttpFixture::new(|request| Reply::Bytes(200, request.body.clone()));
    let output = support::run(
        &format!(
            r#"
        const source = Uint8Array.from([10, 0, 255, 128, 20]);
        const view = source.subarray(1, -1);
        const options = {{ method: "POST", body: view }};
        const response = await fetch("http://{0}/view", {{ ...options }});
        const received = await response.bytes();
        console.log(JSON.stringify(received));
        console.log(received.length);
        console.log(response.status);
        received[0] = 7;
        console.log(source[1]);
        const again = await response.bytes();
        console.log(again !== received);
        console.log(again[0]);
        const empty = await fetch("http://{0}/empty", {{method:"POST", body:new Uint8Array(0)}});
        console.log((await empty.bytes()).length);
        const large = new Uint8Array(131073);
        for (let i = 0; i < large.length; i++) {{ large[i] = i; }}
        const big = await fetch("http://{0}/large", {{method:"PUT", body:large}});
        const bytes = await big.bytes();
        console.log(bytes.length);
        let equal = true;
        for (let i = 0; i < bytes.length; i++) {{ if (bytes[i] !== large[i]) {{ equal = false; }} }}
        console.log(equal);
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "{\"0\":0,\"1\":255,\"2\":128}\n3\n200\n0\ntrue\n0\n0\n131073\ntrue\n"
    );
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].body, [0, 255, 128]);
    assert!(requests[1].body.is_empty());
    assert_eq!(
        requests[2].body,
        (0..131073).map(|index| index as u8).collect::<Vec<_>>()
    );
}

#[test]
fn text_decodes_utf8_with_replacement_without_changing_cached_bytes() {
    let fixture = HttpFixture::new(|request| {
        Reply::Bytes(
            200,
            if request.target == "/json" {
                b"\xef\xbb\xbf{\"value\":\"\xff\"}".to_vec()
            } else {
                vec![239, 187, 191, 65, 255, 66]
            },
        )
    });
    let output = support::run(
        &format!(
            r#"
        const response = await fetch("http://{}/text");
        console.log(await response.text());
        console.log(JSON.stringify(await response.bytes()));
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "A�B\n{\"0\":239,\"1\":187,\"2\":191,\"3\":65,\"4\":255,\"5\":66}\n"
    );
    let output = support::run(
        &format!(
            r#"
        const response = await fetch("http://{}/json");
        console.log(JSON.stringify(await response.json()));
        console.log(await response.text());
    "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "{\"value\":\"�\"}\n{\"value\":\"�\"}\n"
    );
    for method in ["arrayBuffer", "blob", "formData", "clone"] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/unsupported");
            response.{method}(); console.log("unreachable");
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("Unsupported Response method: {method}")),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn repeated_http_calls_retain_live_responses_and_release_dead_resources() {
    let scratch = support::Scratch::new();
    let wit = format!(
        r#"{}
        world test {{
            include runtime-adapter;
            export run-task: func(mode: string) -> string;
        }}
    "#,
        include_str!("../wit/world.wit")
    );
    let compiled = scratch.compile_artifacts(
        r#"
        let saved: any = null;
        export function runTask(mode: string): string {
            if (mode === "saved") { return JSON.stringify(saved.response.bytes()); }
            if (mode === "release") { saved = null; return "released"; }
            const response = fetch("https://fixture.test/binary", {
                method: "POST", body: Uint8Array.from([0, 255, 128])
            });
            if (mode === "pending") { return "pending"; }
            if (mode === "retain") { saved = { response }; return "retained:" + response.status; }
            if (mode === "headers") { return "status:" + response.status; }
            return JSON.stringify(response.bytes());
        }
    "#,
        Some(&wit),
    );
    let path = scratch.0.join("http-lifetimes.wasm");
    fs::write(&path, compiled.core).unwrap();
    let output = Command::new("node").args(["--eval", r#"
        const assert = require('node:assert/strict');
        const module = new WebAssembly.Module(require('node:fs').readFileSync(process.argv[1]));
        let exports;
        let next = 1;
        let failBody = false;
        let diagnostic = '';
        const resources = new Map();
        const payload = Uint8Array.from({length: 4096}, (_, index) => index % 256);
        const expected = JSON.stringify(payload);
        const view = () => new DataView(exports.memory.buffer);
        function resource(type) { const id = next++; resources.set(id, {type, offset:0}); return id; }
        function release(id, type) {
            assert.equal(resources.get(id)?.type, type, `resource ownership: ${id} ${type}`);
            resources.delete(id);
        }
        function result(ptr, id) {
            view().setUint8(ptr, 0);
            if (id !== undefined) { view().setUint32(ptr + 4, id, true); }
        }
        const imports = {};
        for (const {module: namespace, name} of WebAssembly.Module.imports(module)) {
            const host = (...args) => {
                if (name.startsWith('[resource-drop]')) {
                    release(args[0], name.slice('[resource-drop]'.length)); return;
                }
                switch (name) {
                    case '[static]fields.from-list': result(args[2], resource('fields')); return;
                    case '[constructor]outgoing-request':
                        release(args[0], 'fields'); return resource('outgoing-request');
                    case '[method]outgoing-request.set-method':
                    case '[method]outgoing-request.set-scheme':
                    case '[method]outgoing-request.set-authority':
                    case '[method]outgoing-request.set-path-with-query': result(args.at(-1)); return;
                    case '[method]outgoing-request.body': result(args[1], resource('outgoing-body')); return;
                    case '[method]outgoing-body.write': result(args[1], resource('output-stream')); return;
                    case 'handle':
                        release(args[0], 'outgoing-request'); result(args.at(-1));
                        view().setUint32(args.at(-1) + 8, resource('future-incoming-response'), true); return;
                    case '[method]output-stream.blocking-write-and-flush':
                        if (resources.get(args[0]).stderr) {
                            diagnostic += new TextDecoder().decode(new Uint8Array(exports.memory.buffer, args[1], args[2]));
                        } else {
                            assert.deepEqual(Array.from(new Uint8Array(exports.memory.buffer, args[1], args[2])), [0,255,128]);
                        }
                        result(args[3]); return;
                    case 'get-stderr': {
                        const id = resource('output-stream'); resources.get(id).stderr = true; return id;
                    }
                    case 'exit': throw new Error('guest exit');
                    case '[static]outgoing-body.finish': release(args[0], 'outgoing-body'); result(args.at(-1)); return;
                    case '[method]future-incoming-response.subscribe': return resource('pollable');
                    case '[method]pollable.block': return;
                    case '[method]future-incoming-response.get':
                        view().setUint8(args[1], 1);
                        view().setUint8(args[1] + 8, 0);
                        view().setUint8(args[1] + 16, 0);
                        view().setUint32(args[1] + 24, resource('incoming-response'), true); return;
                    case '[method]incoming-response.status': return 200;
                    case '[method]incoming-response.headers': return resource('fields');
                    case '[method]fields.entries':
                        view().setUint32(args[1], 0, true); view().setUint32(args[1] + 4, 0, true); return;
                    case '[method]incoming-response.consume': result(args[1], resource('incoming-body')); return;
                    case '[method]incoming-body.stream': result(args[1], resource('input-stream')); return;
                    case '[method]input-stream.blocking-read': {
                        const stream = resources.get(args[0]);
                        if (failBody && stream.offset > 0) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 0);
                            view().setUint32(args[2] + 8, resource('error'), true); return;
                        }
                        if (stream.offset === payload.length) {
                            view().setUint8(args[2], 1); view().setUint8(args[2] + 4, 1); return;
                        }
                        const chunk = payload.subarray(stream.offset, stream.offset + Math.min(1024, Number(args[1])));
                        const ptr = exports.cabi_realloc(0, 0, 1, chunk.length);
                        new Uint8Array(exports.memory.buffer, ptr, chunk.length).set(chunk);
                        result(args[2], ptr); view().setUint32(args[2] + 8, chunk.length, true);
                        stream.offset += chunk.length; return;
                    }
                    default: throw new Error(`unexpected host import: ${namespace} ${name} ${args}`);
                }
            };
            (imports[namespace] ??= {})[name] = host;
        }
        exports = new WebAssembly.Instance(module, imports).exports;
        function invoke(mode, cleanup = true) {
            const bytes = new TextEncoder().encode(mode);
            const ptr = exports.cabi_realloc(0, 0, 1, bytes.length);
            new Uint8Array(exports.memory.buffer, ptr, bytes.length).set(bytes);
            let ret;
            try { ret = exports['run-task'](ptr, bytes.length); }
            finally { exports.cabi_realloc(ptr, bytes.length, 1, 0); }
            const words = new Uint32Array(exports.memory.buffer, ret, 2);
            const text = new TextDecoder().decode(new Uint8Array(exports.memory.buffer, words[0], words[1]));
            if (cleanup) { exports['cabi_post_run-task'](ret); }
            return text;
        }
        assert.equal(invoke('retain'), 'retained:200');
        assert.ok(resources.size > 0);
        assert.equal(invoke('binary'), expected);
        assert.equal(invoke('saved'), expected);
        assert.equal(invoke('release'), 'released');
        for (let i = 0; i < 30; i++) { assert.equal(invoke('binary'), expected); }
        const baseline = exports.memory.buffer.byteLength;
        for (let i = 0; i < 600; i++) {
            const mode = ['binary', 'pending', 'headers'][i % 3];
            assert.equal(invoke(mode, i % 7 !== 0), mode === 'binary' ? expected : mode === 'pending' ? 'pending' : 'status:200');
            assert.ok(resources.size <= 1, `unreleased HTTP resources: ${resources.size}`);
        }
        assert.equal(invoke('release'), 'released');
        assert.equal(resources.size, 0);
        assert.equal(exports.memory.buffer.byteLength, baseline, 'HTTP cycles must stop growing memory after warm-up');
        failBody = true;
        assert.throws(() => invoke('binary'), /guest exit/);
        assert.match(diagnostic, /Stream error reading response/);
        assert.deepEqual(Array.from(resources.values()).map(resource => resource.type), ['incoming-response']);
        failBody = false;
        assert.equal(invoke('release'), 'released');
        assert.equal(resources.size, 0, 'the next invocation releases the failed response');
        assert.equal(invoke('binary'), expected);
    "#]).arg(&path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn response_headers_and_methods_survive_body_consumption() {
    let fixture = HttpFixture::new(|request| {
        Reply::WithHeaders(
            202,
            vec![
                ("X-Result".into(), request.method.clone()),
                ("X-List".into(), "one".into()),
                ("X-List".into(), "two".into()),
                ("X-Empty".into(), "".into()),
            ],
            "accepted".into(),
        )
    });
    let output = support::run(
        &format!(
            r#"
        const methods = ["put", "PATCH", "patch", "PaTcH", "DELETE", "HEAD", "OPTIONS", "custom"];
        for (let i = 0; i < methods.length; i++) {{
            const response = await fetch("http://{0}/metadata", {{method: methods[i]}});
            console.log(response.status);
            console.log(response.statusText === "");
            const headers = response.headers;
            console.log(headers === response.headers);
            console.log(headers.get("X-RESULT"));
            console.log(headers.has("x-result"));
            console.log(headers.get("missing") === null);
            console.log(headers.get("x-list"));
            console.log(headers.has("missing"));
            console.log(headers.has("x-empty"));
            console.log(headers.get("x-empty") === "");
            console.log(response.url === "http://{0}/metadata");
            console.log(await response.text());
            console.log(response.headers.get("x-result"));
        }}
    "#,
            fixture.address
        ),
        None,
        None,
    );
    let methods = [
        "PUT", "PATCH", "patch", "PaTcH", "DELETE", "HEAD", "OPTIONS", "custom",
    ];
    let expected: String = methods
        .iter()
        .map(|method| {
            format!(
                "202\ntrue\ntrue\n{method}\ntrue\ntrue\none, two\nfalse\ntrue\ntrue\ntrue\n{}\n{method}\n",
                if *method == "HEAD" { "" } else { "accepted" }
            )
        })
        .collect();
    assert_eq!(support::stdout(&output), expected);
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.method.as_str())
            .collect::<Vec<_>>(),
        methods
    );
}

#[test]
fn fetch_in_class_method_selects_http_dispatch() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "class response".into()));
    let output = support::run(
        &format!(
            r#"
            class Client {{ static request() {{ return fetch("http://{}"); }} }}
            const response = await Client.request();
            console.log(await response.text());
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "class response\n");
}

#[test]
fn http_error_statuses_resolve_with_status_and_readable_body() {
    let fixture = HttpFixture::new(|request| {
        Reply::WithHeaders(
            request.target[1..].parse().unwrap(),
            vec![("X-Status".into(), request.target[1..].into())],
            "error body".into(),
        )
    });
    for status in [200, 404, 500] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/{status}");
            console.log(response.status);
            console.log(response.ok);
            console.log(response.headers.get("x-status"));
            console.log(await response.text());
            console.log(response.headers.get("X-STATUS"));
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert_eq!(
            support::stdout(&output),
            format!(
                "{status}\n{}\n{status}\nerror body\n{status}\n",
                status == 200
            )
        );
    }
}

#[test]
fn response_header_errors_report_diagnostics_instead_of_dummy_values() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "ok".into()));
    for (operation, diagnostic) in [
        (r#"response.headers.get("")"#, "Invalid HTTP header name"),
        (
            r#"response.headers.has("bad name")"#,
            "Invalid HTTP header name",
        ),
        (
            r#"response.headers.set("x-name", "value")"#,
            "Unsupported Headers method: set",
        ),
        (
            r#"response.headers.entries()"#,
            "Unsupported Headers method: entries",
        ),
    ] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/");
            {operation};
            console.log("continued");
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success(), "{operation}");
        assert!(output.stdout.is_empty(), "{operation}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{operation}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), 4);
}

#[test]
fn response_json_preserves_primitive_and_composite_values() {
    let fixture = HttpFixture::new(|request| {
        let body = match request.target.as_str() {
            "/number" => "42",
            "/boolean" => "true",
            "/string" => "\"hello\"",
            "/null" => "null",
            "/array" => "[7]",
            "/object" => "{\"value\":8}",
            _ => panic!("unexpected request: {request:?}"),
        };
        Reply::Body(200, body.into())
    });
    let output = support::run(
        &format!(
            r#"
            const numberResponse = await fetch("http://{0}/number");
            const numberValue = await numberResponse.json();
            console.log(numberValue * 2);
            console.log(numberValue + 1);
            const booleanResponse = await fetch("http://{0}/boolean");
            console.log((await booleanResponse.json()) === true);
            const stringResponse = await fetch("http://{0}/string");
            console.log((await stringResponse.json()) + "!");
            const nullResponse = await fetch("http://{0}/null");
            console.log((await nullResponse.json()) === null);
            const arrayResponse = await fetch("http://{0}/array");
            const arrayValue = await arrayResponse.json();
            console.log(arrayValue[0] * 2);
            const objectResponse = await fetch("http://{0}/object");
            const objectValue = await objectResponse.json();
            console.log(objectValue.value * 2);
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(
        support::stdout(&output),
        "84\n43\ntrue\nhello!\ntrue\n14\n16\n"
    );
}

#[test]
fn fetch_forwards_static_and_dynamic_options_and_complete_bodies() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "accepted".into()));
    let body = "é".repeat(5000);
    let output = support::run(
        &format!(
            r#"
            const first = await fetch("http://{0}/submit?key=value#ignored", {{
                method: "POST", headers: {{ "x-static": "yes" }}, body: "{body}"
            }});
            console.log(await first.text());
            const headers = {{ "x-dynamic": "yes", "content-type": "text/plain" }};
            const options = {{ method: "post", headers: headers, body: "second" }};
            const second = await fetch("http://{0}", options);
            console.log(await second.text());
            const third = await fetch("http://{0}?get=yes", {{ headers: [["x-pair", "yes"]] }});
            console.log(await third.text());
        "#,
            fixture.address
        ),
        None,
        None,
    );
    assert_eq!(support::stdout(&output), "accepted\naccepted\naccepted\n");
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/submit?key=value");
    assert_eq!(requests[0].body, body.as_bytes());
    assert!(
        requests[0]
            .headers
            .contains(&("x-static".into(), "yes".into()))
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].body, b"second");
    assert!(
        requests[1]
            .headers
            .contains(&("x-dynamic".into(), "yes".into()))
    );
    assert!(
        requests[1]
            .headers
            .contains(&("content-type".into(), "text/plain".into()))
    );
    assert_eq!(requests[2].method, "GET");
    assert_eq!(requests[2].target, "/?get=yes");
    assert!(
        requests[2]
            .headers
            .contains(&("x-pair".into(), "yes".into()))
    );
}

#[test]
fn unsupported_fetch_options_fail_before_sending_a_request() {
    let fixture = HttpFixture::new(|_| Reply::Body(200, "unexpected".into()));
    for (options, message) in [
        (r#"{ cache: "reload" }"#, "Unsupported fetch option: cache"),
        (
            r#"{ redirect: "follow" }"#,
            "Unsupported fetch option: redirect",
        ),
        ("{ signal: null }", "Unsupported fetch option: signal"),
        (r#"{ method: "CONNECT" }"#, "Forbidden fetch method"),
        (r#"{ method: "bad method" }"#, "Invalid fetch method"),
        (r#"{ method: "POST", body: 42 }"#, "body must be a string"),
        (
            r#"{ method: "POST", body: [0,255] }"#,
            "body must be a string or Uint8Array",
        ),
        (
            r#"{ method: "POST", body: { "0": 255 } }"#,
            "body must be a string or Uint8Array",
        ),
        (
            "{ headers: { wrong: 42 } }",
            "header values must be strings",
        ),
        (
            r#"{ body: "invalid GET body" }"#,
            "GET and HEAD requests cannot have a body",
        ),
        (
            r#"{ method: "HEAD", body: "" }"#,
            "GET and HEAD requests cannot have a body",
        ),
        (
            "{ body: new Uint8Array(0) }",
            "GET and HEAD requests cannot have a body",
        ),
        (
            "{ method: 'HEAD', body: Uint8Array.from([1]) }",
            "GET and HEAD requests cannot have a body",
        ),
    ] {
        let output = support::run(
            &format!(
                r#"await fetch("http://{}/", {options}); console.log("continued");"#,
                fixture.address
            ),
            None,
            None,
        );
        assert!(!output.status.success(), "options should fail: {options}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(fixture.requests.lock().unwrap().is_empty());
}

fn run_bounded(wasm: &Path) -> Output {
    let mut child = Command::new(support::get_wasmtime_path())
        .args([
            "run",
            "-C",
            "cache=n",
            "-S",
            "http=y",
            "-S",
            "inherit-network=y",
        ])
        .arg(wasm)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "request remained blocked: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn promise_all_observes_later_failure_while_earlier_request_stalls() {
    let fixture = HttpFixture::new(|request| match request.target.as_str() {
        "/stall" => Reply::Stall,
        "/body" => Reply::StallBody,
        "/fail" => Reply::Disconnect,
        _ => panic!("unexpected request: {request:?}"),
    });
    for path in ["stall", "body"] {
        let scratch = support::Scratch::new();
        let wasm = scratch.compile(
            &format!(
                r#"
                await Promise.all([fetch("http://{0}/{path}"), fetch("http://{0}/fail")]);
                console.log("continued");
            "#,
                fixture.address
            ),
            None,
        );
        let output = run_bounded(&wasm);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("HTTP request") && error.contains("/fail"),
            "{error}"
        );
    }
}

#[test]
fn promise_all_retains_input_order_and_fetch_resolves_before_body_completion() {
    let fixture = HttpFixture::new(|request| match request.target.as_str() {
        "/first" => {
            thread::sleep(Duration::from_millis(100));
            Reply::Body(200, "first".into())
        }
        "/second" => Reply::Body(200, "second".into()),
        "/body" => Reply::StallBody,
        _ => panic!("unexpected request: {request:?}"),
    });
    let scratch = support::Scratch::new();
    let wasm = scratch.compile(
        &format!(
            r#"
            const first = fetch("http://{0}/first");
            const second = fetch("http://{0}/second");
            const responses = await Promise.all([first, second, first]);
            console.log(await responses[0].text());
            console.log(await responses[1].text());
            console.log(await responses[2].text());
            const empty = await Promise.all([]);
            console.log(empty.length);
            const response = await fetch("http://{0}/body");
            console.log(response.status);
        "#,
            fixture.address
        ),
        None,
    );
    assert_eq!(
        support::stdout(&run_bounded(&wasm)),
        "first\nsecond\nfirst\n0\n200\n"
    );
}
