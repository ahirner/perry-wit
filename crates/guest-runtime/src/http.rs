//! WASI HTTP outgoing requests and asynchronous stream consumption.

use crate::bindings;
use crate::bindings::wasi::http::outgoing_handler::handle;
use crate::bindings::wasi::http::types::{
    Fields, FutureIncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};

pub(crate) enum ResponseEntry {
    InFlight {
        url: String,
        future_resp: FutureIncomingResponse,
    },
    Ready(String),
}

impl ResponseEntry {
    pub(crate) fn resolve(&mut self) -> Result<&str, String> {
        match self {
            ResponseEntry::Ready(s) => Ok(s.as_str()),
            ResponseEntry::InFlight { url, future_resp } => {
                let pollable = future_resp.subscribe();
                pollable.block();

                let response = future_resp
                    .get()
                    .ok_or_else(|| format!("Response for {url} not available"))?
                    .map_err(|_| format!("Response for {url} already consumed"))?
                    .map_err(|e| format!("HTTP request to '{url}' failed: {e:?}"))?;

                let status = response.status();
                if status < 200 || status >= 300 {
                    return Err(format!("HTTP request to '{url}' returned status {status}"));
                }

                let body = response
                    .consume()
                    .map_err(|_| format!("Failed to consume response body for {url}"))?;
                let stream = body
                    .stream()
                    .map_err(|_| format!("Failed to get response stream for {url}"))?;

                let mut content = Vec::new();
                loop {
                    match stream.blocking_read(8192) {
                        Ok(chunk) => {
                            if chunk.is_empty() {
                                break;
                            }
                            content.extend_from_slice(&chunk);
                        }
                        Err(bindings::wasi::io::streams::StreamError::Closed) => {
                            break;
                        }
                        Err(e) => {
                            return Err(format!("Stream error reading response from {url}: {e:?}"));
                        }
                    }
                }

                let body_str = String::from_utf8(content)
                    .map_err(|e| format!("Response from {url} was not UTF-8: {e}"))?;
                drop(stream);
                drop(body);
                drop(response);
                drop(pollable);

                *self = ResponseEntry::Ready(body_str);
                match self {
                    ResponseEntry::Ready(s) => Ok(s.as_str()),
                    _ => unreachable!(),
                }
            }
        }
    }
}

pub(crate) fn start_http_get(url: &str) -> Result<FutureIncomingResponse, String> {
    let (scheme, authority, path) = split_url(url)?;

    let headers = Fields::new();
    let request = OutgoingRequest::new(headers);
    request
        .set_method(&Method::Get)
        .map_err(|_| "Failed to set method")?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|_| "Failed to set scheme")?;
    request
        .set_authority(Some(&authority))
        .map_err(|_| "Failed to set authority")?;
    request
        .set_path_with_query(Some(&path))
        .map_err(|_| "Failed to set path")?;

    let outgoing_body = request.body().map_err(|_| "Failed to get request body")?;
    OutgoingBody::finish(outgoing_body, None).map_err(|_| "Failed to finish outgoing body")?;

    let future_resp = handle(request, None).map_err(|e| format!("HTTP handle error: {e:?}"))?;
    Ok(future_resp)
}

fn split_url(url: &str) -> Result<(Scheme, String, String), String> {
    let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (Scheme::Https, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (Scheme::Http, rest)
    } else {
        return Err(format!("Unsupported scheme in URL: {url}"));
    };

    let (authority, path) = match rest.find('/') {
        Some(pos) => (&rest[..pos], &rest[pos..]),
        None => (rest, "/"),
    };

    Ok((scheme, authority.to_string(), path.to_string()))
}
