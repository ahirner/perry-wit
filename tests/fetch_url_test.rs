#[path = "../src/helpers/fetch/url.rs"]
mod url;

#[test]
fn http_url_normalization_matches_node_for_host_path_and_query_shapes() {
    let cases = [
        "HTTP://Example.COM:00080?q=hello world#ignored",
        "https://example.com:0443",
        "http://example.com:00081/",
        "https://example.com:/",
        "http://[0:0:0:0:0:0:0:1]:80/a",
        "https://[2001:0DB8:0:1:0:0:0:1]:8443/é",
        "http://[::ffff:192.0.2.1]/",
        "http://[1:0:0:2:0:0:3:4]/",
        "http://[::]/",
        "http://127.1/",
        "http://0x7f000001/",
        "http://0177.0.0.1/",
        "http://1.2.65535/",
        "http://exam%70le.com/a",
        "http://./",
        "http://example../",
        "http://hello_/",
        "http://example.com/a/./b/../c/",
        "http://example.com/a/%2e/%2e%2E/c",
        "http://example.com/a/.%2e",
        "http://example.com/a/%2e.",
        "http://example.com/a/..",
        "http://example.com/../../x",
        "http://example.com/a//../",
        "http://example.com/a/.//x",
        "http://example.com/a/.",
        "http://example.com/a//./",
        "http://example.com/a/b/../..",
        "http://example.com/a\\b\\..\\c?key=\\value",
        "http://example.com/é🙂/^`{}?key='{}^`é🙂#gone",
        "http://example.com/a%2fb/%gg?a=%ff&plus=+",
        "http://example.com?",
        "https://example.com#?discarded",
        "  http://exa\tmple.com/a\r/b\n?q=a b  ",
        "\0http://example.com/\0",
    ];
    let script = "for(const source of JSON.parse(process.argv[1])) {const u=new URL(source);u.hash='';console.log(u.href)}";
    let output = std::process::Command::new("node")
        .args(["-e", script, &serde_json::to_string(&cases[..]).unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = String::from_utf8(output.stdout).unwrap();
    for (input, expected) in cases.iter().zip(expected.lines()) {
        let mut bytes = vec![0; input.len() * 4 + 64];
        let metadata =
            url::normalize(input.as_bytes(), &mut bytes).unwrap_or_else(|_| panic!("{input:?}"));
        assert_eq!(
            std::str::from_utf8(&bytes[..metadata[4] as usize]).unwrap(),
            expected,
            "{input:?}"
        );
        assert_eq!(metadata[2], metadata[3]);
        assert_eq!(bytes[metadata[3] as usize], b'/');
        let canonical = bytes[..metadata[4] as usize].to_vec();
        assert_eq!(
            url::normalize(&canonical, &mut bytes).unwrap(),
            metadata,
            "idempotence: {input:?}"
        );
        assert_eq!(&bytes[..canonical.len()], canonical);
    }
}

#[test]
fn invalid_http_authorities_and_short_outputs_fail_before_io() {
    for input in [
        "http://",
        "file:///a",
        "http://user@host",
        "http://host:65536",
        "http://host:x",
        "http://[::1",
        "http://[::1]tail",
        "http://host|name/",
        "http://a%2fb/",
        "http://a%00b/",
        "http://a%zzb/",
        "http://1.2.3.256/",
        "http://test.42/",
        "http://1.2.3.4.5/",
        "http://09/",
    ] {
        assert!(
            url::normalize(input.as_bytes(), &mut [0; 512]).is_err(),
            "{input}"
        );
    }
    let source = b"http://example.com/";
    for capacity in 0..source.len() {
        assert!(url::normalize(source, &mut vec![0; capacity]).is_err());
    }
}

#[test]
fn relative_redirect_locations_match_node() {
    let base = "http://example.com/a/b?old=yes";
    let locations = [
        "../c",
        "./c?x=1#hash",
        "/x/../z",
        "//other.example:80/x",
        "https://other.example",
        "?new=query",
        "#fragment",
        "",
        "  next  ",
        "\\path\\next",
        "é?q='{}",
    ];
    let script = "for(const ref of JSON.parse(process.argv[2])) {const u=new URL(ref,process.argv[1]);u.hash='';console.log(u.href)}";
    let output = std::process::Command::new("node")
        .args([
            "-e",
            script,
            base,
            &serde_json::to_string(&locations).unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let expected = String::from_utf8(output.stdout).unwrap();
    for (location, expected) in locations.iter().zip(expected.lines()) {
        let mut output = [0; 4096];
        let metadata = url::resolve(base.as_bytes(), location.as_bytes(), &mut output).unwrap();
        assert_eq!(
            std::str::from_utf8(&output[..metadata[4] as usize]).unwrap(),
            expected,
            "{location:?}"
        );
    }
}
