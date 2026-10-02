#[path = "support/http_fixture.rs"]
mod http_fixture;
mod support;

use http_fixture::{HttpFixture, Reply};

#[test]
fn http_error_statuses_resolve_with_status_and_readable_body() {
    let fixture = HttpFixture::new(|request| {
        Reply::Body(request.target[1..].parse().unwrap(), "error body".into())
    });
    for status in [200, 404, 500] {
        let output = support::run(
            &format!(
                r#"
            const response = await fetch("http://{}/{status}");
            console.log(response.status);
            console.log(response.ok);
            console.log(await response.text());
        "#,
                fixture.address
            ),
            None,
            None,
        );
        assert_eq!(
            support::stdout(&output),
            format!("{status}\n{}\nerror body\n", status == 200)
        );
    }
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
    assert_eq!(support::stdout(&output), "84\ntrue\nhello!\ntrue\n14\n16\n");
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
        (r#"{ method: "PUT" }"#, "only GET and POST"),
        (r#"{ method: "POST", body: 42 }"#, "body must be a string"),
        (
            "{ headers: { wrong: 42 } }",
            "header values must be strings",
        ),
        (
            r#"{ body: "invalid GET body" }"#,
            "GET requests cannot have a body",
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
