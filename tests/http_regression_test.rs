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
