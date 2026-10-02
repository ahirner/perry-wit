#[path = "../crates/guest-runtime/src/http_url.rs"]
mod http_url;

use http_url::{HttpScheme, parse_http_url};

#[test]
fn request_targets_keep_queries_and_exclude_fragments() {
    for (url, authority, target) in [
        ("https://example.com", "example.com", "/"),
        (
            "https://example.com?key=value",
            "example.com",
            "/?key=value",
        ),
        (
            "https://example.com?key=value#fragment",
            "example.com",
            "/?key=value",
        ),
        ("https://example.com#fragment", "example.com", "/"),
        (
            "https://example.com/path?q=%23value#fragment?ignored",
            "example.com",
            "/path?q=%23value",
        ),
        ("https://[::1]:8443?empty=", "[::1]:8443", "/?empty="),
    ] {
        let parsed = parse_http_url(url).unwrap();
        assert_eq!(parsed.scheme, HttpScheme::Https);
        assert_eq!(parsed.authority, authority);
        assert_eq!(parsed.path_with_query, target);
    }
    assert_eq!(
        parse_http_url("HTTP://localhost/").unwrap().scheme,
        HttpScheme::Http
    );
    for url in [
        "file:///tmp",
        "https:///path",
        "https://?key=value",
        "http://user:pass@example.com",
    ] {
        assert!(parse_http_url(url).is_err(), "{url}");
    }
}
