#[path = "support/http_fixture.rs"]
mod http_fixture;
mod support;

use http_fixture::{HttpFixture, Reply};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

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
    assert_eq!(requests[0].body, body);
    assert!(
        requests[0]
            .headers
            .contains(&("x-static".into(), "yes".into()))
    );
    assert_eq!(requests[1].method, "POST");
    assert_eq!(requests[1].body, "second");
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
