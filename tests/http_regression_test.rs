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
