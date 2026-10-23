//! `perry:http-handler` programs served through `wasmtime-wasi-http`'s P3 `Service` with the
//! request shapes a server such as `wasmtime serve` produces: origin-form targets without a
//! scheme or authority, bodiless requests and several headers.
use crate::{
    engine::async_engine,
    wasi_host::{self, Host},
};
use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use std::path::Path;
use wasmtime::component::Component;

/// Compile `source` against the `perry:http-handler` world the SDK documents.
fn handler(source: &str) -> Result<Vec<u8>> {
    let (resolve, package) = perry_wit::component::wit::resolve_wit(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/http-handler"),
    )?;
    let world = resolve.select_world(&[package], Some("handler"))?;
    let compiled = perry_wit::compile_for_world(source, "handler.ts", &resolve, world, true)?;
    Ok(compiled
        .component
        .ok_or_else(|| anyhow!("component output"))?
        .bytes)
}

/// Send `requests` to one instance in order; the status and body of each response, or 500 and
/// the error code the handler returned.
async fn exchange(
    component: &[u8],
    requests: Vec<http::Request<Full<Bytes>>>,
) -> Result<Vec<(u16, Vec<u8>)>> {
    let engine = async_engine()?;
    let component = Component::new(&engine, component)?;
    let mut store = wasi_host::store(&engine, Host::default());
    let linker = wasi_host::linker(&engine)?;
    let service = wasmtime_wasi_http::p3::bindings::Service::instantiate_async(
        &mut store, &component, &linker,
    )
    .await?;
    let mut responses = Vec::new();
    for request in requests {
        let (request, completed) = wasmtime_wasi_http::p3::Request::from_http(
            wasmtime_wasi_http::default_hooks(),
            request,
        );
        let response = store
            .run_concurrent(async |accessor| -> Result<(u16, Vec<u8>)> {
                // A server such as `wasmtime serve` answers an error code with status 500.
                let response = match service.handle(accessor, request).await? {
                    Ok(response) => response,
                    Err(code) => return Ok((500, format!("{code:?}").into_bytes())),
                };
                let response =
                    accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
                let status = response.status().as_u16();
                let body = Vec::from(response.into_body().collect().await?.to_bytes());
                completed.await?;
                Ok((status, body))
            })
            .await??;
        responses.push(response);
    }
    Ok(responses)
}

fn request(method: &str, uri: &str, body: &'static [u8]) -> Result<http::Request<Full<Bytes>>> {
    Ok(http::Request::builder()
        .method(method)
        .uri(uri)
        .header("user-agent", "curl/8.0")
        .header("accept", "*/*")
        .body(Full::new(Bytes::from_static(body)))?)
}

/// Origin-form and absolute targets, with and without bodies, reach the handler intact, and a
/// throwing request answers 500 while the instance keeps serving.
#[tokio::test(flavor = "current_thread")]
async fn handler_serves_origin_form_and_bodiless_requests() -> Result<()> {
    let component = handler(
        r#"
import type { Request, Response } from "./handler-types/world";

const encoder = new TextEncoder();

export function handle(request: Request): Response {
  const path = request.pathWithQuery ?? "/";
  if (path === "/fail") throw new Error("handler failed");
  const scheme = request.scheme === null ? "none" : request.scheme.tag;
  const authority = request.authority ?? "none";
  return {
    status: 200,
    headers: [["content-type", encoder.encode("text/plain")]],
    body: encoder.encode(
      request.method.tag + " " + scheme + " " + authority + " " + path + " " +
        String(request.headers.length) + " " + String(request.body.length),
    ),
  };
}
"#,
    )?;
    let responses = exchange(
        &component,
        vec![
            request("GET", "/hello?x=1", b"")?,
            request("POST", "/submit", b"body")?,
            request("GET", "/fail", b"")?,
            request("GET", "http://example.test/absolute", b"")?,
        ],
    )
    .await?;
    let text = responses
        .into_iter()
        .map(|(status, body)| Ok((status, String::from_utf8(body).context("utf-8 body")?)))
        .collect::<Result<Vec<_>>>()?;
    assert_eq!(
        text,
        [
            (200, "get none none /hello?x=1 2 0".to_owned()),
            (200, "post none none /submit 2 4".to_owned()),
            (
                500,
                r#"ErrorCode::InternalError(Some("Error: handler failed"))"#.to_owned(),
            ),
            (200, "get http example.test /absolute 2 0".to_owned()),
        ]
    );
    Ok(())
}
