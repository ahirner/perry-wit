//! WASI HTTP outgoing requests and asynchronous stream consumption.

use crate::bindings;
use crate::bindings::wasi::http::outgoing_handler::handle;
use crate::bindings::wasi::http::types::{
    Fields, FutureIncomingResponse, IncomingResponse, Method, OutgoingBody, OutgoingRequest, Scheme,
};

pub(crate) enum ResponseEntry {
    Vacant,
    InFlight {
        url: String,
        future_resp: FutureIncomingResponse,
    },
    Headers {
        url: String,
        response: IncomingResponse,
    },
    Ready {
        body: Vec<u8>,
        metadata: ResponseMetadata,
    },
}

#[derive(Clone)]
pub(crate) struct ResponseMetadata {
    pub(crate) url: String,
    pub(crate) status: u16,
    pub(crate) headers: Vec<(String, String)>,
}

static mut RESPONSES: Option<Vec<ResponseEntry>> = None;

pub(crate) fn get_responses() -> &'static mut Vec<ResponseEntry> {
    unsafe {
        let resp_ptr = core::ptr::addr_of_mut!(RESPONSES);
        if (*resp_ptr).is_none() {
            *resp_ptr = Some(Vec::new());
        }
        (*resp_ptr).as_mut().unwrap()
    }
}

pub(crate) fn get_response_body(id: usize) -> Result<Vec<u8>, String> {
    let responses = get_responses();
    if id < responses.len() {
        let res = responses[id].resolve()?.to_vec();
        Ok(res)
    } else {
        Err(format!("Invalid response id {id}"))
    }
}

pub(crate) fn decode_utf8_body(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned()
}

pub(crate) fn store_response(response: ResponseEntry) -> usize {
    let responses = get_responses();
    if let Some(id) = responses
        .iter()
        .position(|entry| matches!(entry, ResponseEntry::Vacant))
    {
        responses[id] = response;
        id
    } else {
        let id = responses.len();
        responses.push(response);
        id
    }
}

/// Releases unreferenced responses at an ordinary invocation boundary.
/// Component post-return cannot call host imports, including resource drops.
#[no_mangle]
pub(crate) extern "C" fn http_reclaim_responses() {
    let responses = get_responses();
    let mut alive = vec![false; responses.len()];
    for handle in &crate::state::get_state().handles {
        if let crate::state::JsHandle::Response { id, .. } = handle {
            if let Some(alive) = alive.get_mut(*id) {
                *alive = true;
            }
        }
    }
    for (id, entry) in responses.iter_mut().enumerate() {
        if !alive[id] {
            *entry = ResponseEntry::Vacant;
        }
    }
    while matches!(responses.last(), Some(ResponseEntry::Vacant)) {
        responses.pop();
    }
}

impl ResponseEntry {
    pub(crate) fn wait(&mut self) -> Result<(), String> {
        if matches!(self, Self::Vacant) {
            return Err("Response has been released".into());
        }
        if let Self::InFlight { future_resp, .. } = self {
            let pollable = future_resp.subscribe();
            pollable.block();
        }
        self.receive()
    }

    fn receive(&mut self) -> Result<(), String> {
        if let Self::InFlight { url, future_resp } = self {
            let response = future_resp
                .get()
                .ok_or_else(|| format!("Response for {url} not available"))?
                .map_err(|_| format!("Response for {url} already consumed"))?
                .map_err(|error| format!("HTTP request to '{url}' failed: {error:?}"))?;
            *self = Self::Headers {
                url: std::mem::take(url),
                response,
            };
        }
        Ok(())
    }

    pub(crate) fn resolve(&mut self) -> Result<&[u8], String> {
        let metadata = self.metadata()?;
        if let Self::Headers { url, response } = self {
            let body = response
                .consume()
                .map_err(|_| format!("Failed to consume response body for {url}"))?;
            let stream = body
                .stream()
                .map_err(|_| format!("Failed to get response stream for {url}"))?;
            let mut content = Vec::new();
            loop {
                match stream.blocking_read(8192) {
                    Ok(chunk) => content.extend_from_slice(&chunk),
                    Err(bindings::wasi::io::streams::StreamError::Closed) => break,
                    Err(error) => {
                        return Err(format!(
                            "Stream error reading response from {url}: {error:?}"
                        ))
                    }
                }
            }
            drop(stream);
            drop(body);
            *self = Self::Ready {
                body: content,
                metadata,
            };
        }
        match self {
            Self::Ready { body, .. } => Ok(body),
            _ => unreachable!(),
        }
    }

    pub(crate) fn status(&mut self) -> Result<u16, String> {
        self.wait()?;
        match self {
            Self::Headers { response, .. } => Ok(response.status()),
            Self::Ready { metadata, .. } => Ok(metadata.status),
            Self::InFlight { .. } | Self::Vacant => unreachable!(),
        }
    }

    pub(crate) fn metadata(&mut self) -> Result<ResponseMetadata, String> {
        self.wait()?;
        match self {
            Self::Headers { url, response } => Ok(ResponseMetadata {
                url: url.clone(),
                status: response.status(),
                headers: response
                    .headers()
                    .entries()
                    .into_iter()
                    .map(|(name, value)| {
                        (
                            name.to_ascii_lowercase(),
                            value.into_iter().map(char::from).collect(),
                        )
                    })
                    .collect(),
            }),
            Self::Ready { metadata, .. } => Ok(metadata.clone()),
            Self::InFlight { .. } | Self::Vacant => unreachable!(),
        }
    }
}

pub(crate) fn wait_for_responses(
    responses: &mut [ResponseEntry],
    ids: &[usize],
) -> Result<(), String> {
    loop {
        let pending: Vec<_> = ids
            .iter()
            .copied()
            .filter(|&id| matches!(responses[id], ResponseEntry::InFlight { .. }))
            .collect();
        if pending.is_empty() {
            return Ok(());
        }
        let ready = {
            let pollables: Vec<_> = pending
                .iter()
                .map(|&id| match &responses[id] {
                    ResponseEntry::InFlight { future_resp, .. } => future_resp.subscribe(),
                    _ => unreachable!(),
                })
                .collect();
            let borrowed: Vec<_> = pollables.iter().collect();
            bindings::wasi::io::poll::poll(&borrowed)
        };
        for index in ready {
            responses[pending[index as usize]].receive()?;
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
        crate::http_options::RequestMethod::Head => Method::Head,
        crate::http_options::RequestMethod::Put => Method::Put,
        crate::http_options::RequestMethod::Patch => Method::Patch,
        crate::http_options::RequestMethod::Delete => Method::Delete,
        crate::http_options::RequestMethod::Options => Method::Options,
        crate::http_options::RequestMethod::Other(method) => Method::Other(method),
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
