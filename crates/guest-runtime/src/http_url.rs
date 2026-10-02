use std::borrow::Cow;

#[derive(Debug, PartialEq)]
pub(crate) enum HttpScheme {
    Http,
    Https,
}

#[derive(Debug, PartialEq)]
pub(crate) struct HttpUrl<'a> {
    pub(crate) scheme: HttpScheme,
    pub(crate) authority: &'a str,
    pub(crate) path_with_query: Cow<'a, str>,
}

pub(crate) fn parse_http_url(url: &str) -> Result<HttpUrl<'_>, String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or_else(|| format!("Invalid HTTP URL: {url}"))?;
    let scheme = if scheme.eq_ignore_ascii_case("https") {
        HttpScheme::Https
    } else if scheme.eq_ignore_ascii_case("http") {
        HttpScheme::Http
    } else {
        return Err(format!("Unsupported scheme in URL: {url}"));
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err("HTTP URL requires an authority".into());
    }
    if authority.contains('@') {
        return Err("HTTP URL credentials are unsupported".into());
    }
    let target = rest[authority_end..].split('#').next().unwrap();
    let path_with_query = if target.starts_with('/') {
        Cow::Borrowed(target)
    } else {
        Cow::Owned(format!("/{target}"))
    };
    Ok(HttpUrl {
        scheme,
        authority,
        path_with_query,
    })
}
