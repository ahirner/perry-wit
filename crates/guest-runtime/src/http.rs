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
    Ready {
        body: String,
        status: u16,
    },
}

impl ResponseEntry {
    pub(crate) fn resolve(&mut self) -> Result<&str, String> {
        match self {
            ResponseEntry::Ready { body, .. } => Ok(body.as_str()),
            ResponseEntry::InFlight { url, future_resp } => {
                let pollable = future_resp.subscribe();
                pollable.block();

                let response = future_resp
                    .get()
                    .ok_or_else(|| format!("Response for {url} not available"))?
                    .map_err(|_| format!("Response for {url} already consumed"))?
                    .map_err(|e| format!("HTTP request to '{url}' failed: {e:?}"))?;

                let status = response.status();

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

                *self = ResponseEntry::Ready {
                    body: body_str,
                    status,
                };
                match self {
                    ResponseEntry::Ready { body, .. } => Ok(body.as_str()),
                    _ => unreachable!(),
                }
            }
        }
    }

    pub(crate) fn status(&mut self) -> Result<u16, String> {
        self.resolve()?;
        match self {
            Self::Ready { status, .. } => Ok(*status),
            _ => unreachable!(),
        }
    }
}

pub(crate) fn start_http_request(
    url: &str,
    mut options: crate::http_options::RequestOptions,
) -> Result<FutureIncomingResponse, String> {
    let parsed = crate::http_url::parse_http_url(url)?;
    let scheme = match parsed.scheme {
        crate::http_url::HttpScheme::Http => Scheme::Http,
        crate::http_url::HttpScheme::Https => Scheme::Https,
    };

    if !options
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"))
    {
        options.headers.push((
            "content-length".into(),
            options.body.len().to_string().into_bytes(),
        ));
    }
    let headers = Fields::from_list(&options.headers)
        .map_err(|error| format!("Invalid request headers: {error:?}"))?;
    let request = OutgoingRequest::new(headers);
    let method = match options.method {
        crate::http_options::RequestMethod::Get => Method::Get,
        crate::http_options::RequestMethod::Post => Method::Post,
    };
    request
        .set_method(&method)
        .map_err(|_| "Failed to set method")?;
    request
        .set_scheme(Some(&scheme))
        .map_err(|_| "Failed to set scheme")?;
    request
        .set_authority(Some(parsed.authority))
        .map_err(|_| "Failed to set authority")?;
    request
        .set_path_with_query(Some(&parsed.path_with_query))
        .map_err(|_| "Failed to set path")?;

    let outgoing_body = request.body().map_err(|_| "Failed to get request body")?;
    let stream = outgoing_body
        .write()
        .map_err(|_| "Failed to open request body stream")?;
    let future_resp = handle(request, None).map_err(|e| format!("HTTP handle error: {e:?}"))?;
    for chunk in options.body.chunks(4096) {
        stream
            .blocking_write_and_flush(chunk)
            .map_err(|error| format!("Failed to write request body: {error:?}"))?;
    }
    drop(stream);
    OutgoingBody::finish(outgoing_body, None)
        .map_err(|error| format!("Failed to finish outgoing body: {error:?}"))?;
    Ok(future_resp)
}
