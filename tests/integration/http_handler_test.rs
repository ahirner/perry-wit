//! HTTP handlers: a `perry:http-handler` program (`wit/http-handler`) compiles to a
//! `wasi:http/handler@0.3.0` component. One instance serves several exchanges; an exception
//! becomes `internal-error` without ending the instance.
use anyhow::{Result, anyhow};
use http_body_util::{BodyExt, Full};
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    AsContextMut, Store,
    component::{Accessor, Component, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{Request, bindings::Service, bindings::http::types::ErrorCode};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::engine::async_engine;

struct Host {
    ctx: WasiCtx,
    http: WasiHttpCtx,
    table: ResourceTable,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: Default::default(),
        }
    }
}

const SOURCE: &str = r#"
import { setTimeout } from 'node:timers/promises';
import type { PerryHttpHandlerTypesRequest as Request, PerryHttpHandlerTypesResponse as Response }
  from './handler-types/world';

export async function handle(request: Request): Promise<Response> {
  await setTimeout(1);
  const path = request.pathWithQuery;
  if (path === '/throw') throw new TypeError('no such route');
  const method = request.method;
  const scheme = request.scheme;
  let schemeName = 'none';
  if (scheme !== undefined && scheme !== null) schemeName = scheme.tag;
  const other = method.val;
  const verb = other === undefined ? method.tag : method.tag + ':' + other;
  if (path === '/bad-header') {
    return { status: 200, headers: [['bad name', new TextEncoder().encode('x')]], body: new Uint8Array(0) };
  }
  const headers: [string, Uint8Array][] = [];
  for (let i = 0; i < request.headers.length; i++) {
    const header = request.headers[i];
    if (header[0] === 'x-value') headers.push(['x-seen', header[1]]);
  }
  headers.push(['x-request', new TextEncoder().encode(verb + ' ' + schemeName + ' ' + request.authority + ' ' + path)]);
  return { status: path === '/missing' ? 404 : 201, headers, body: request.body };
}
"#;

async fn exchange(
    store: &mut Store<Host>,
    service: &Service,
    method: &str,
    uri: &str,
    body: Vec<u8>,
) -> Result<std::result::Result<(u16, Vec<(String, String)>, Vec<u8>), ErrorCode>> {
    let request = http::Request::builder()
        .method(method)
        .uri(uri)
        .header("x-value", "first")
        .header("x-value", "second")
        .body(Full::new(bytes::Bytes::from(body)))?;
    let (request, completed) = Request::from_http(wasmtime_wasi_http::default_hooks(), request);
    store
        .run_concurrent(async |accessor: &Accessor<Host>| {
            let response = match service.handle(accessor, request).await? {
                Ok(response) => response,
                Err(error) => return Ok(Err(error)),
            };
            let response =
                accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
            let status = response.status().as_u16();
            let headers = response
                .headers()
                .iter()
                .map(|(n, v)| (n.to_string(), String::from_utf8_lossy(v.as_bytes()).into()))
                .collect();
            let bytes = response.into_body().collect().await?.to_bytes().to_vec();
            completed.await?;
            while accessor.with(|mut store| store.as_context_mut().concurrent_state_table_size())
                != 0
            {
                tokio::task::yield_now().await;
            }
            Ok(Ok((status, headers, bytes)))
        })
        .await?
}

#[tokio::test(flavor = "current_thread")]
async fn handlers_serve_exchanges_and_report_exceptions() -> Result<()> {
    let wit = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("wit/http-handler");
    let (resolve, package) = resolve_wit(&wit)?;
    let world = resolve.select_world(&[package], Some("handler"))?;
    let compiled = compile_for_world(SOURCE, "handler.ts", &resolve, world, true)?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder().build(),
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
        },
    );
    let service = Service::instantiate_async(&mut store, &component, &linker).await?;

    // A large body, request headers seen in order, and the request's parts.
    let large: Vec<u8> = (0..1_048_576u32).map(|i| (i * 7) as u8).collect();
    let (status, headers, body) = exchange(
        &mut store,
        &service,
        "PUT",
        "https://example.test/echo?q=1",
        large.clone(),
    )
    .await?
    .map_err(|e| anyhow!("{e:?}"))?;
    assert_eq!(status, 201);
    assert_eq!(body, large);
    let seen: Vec<_> = headers
        .iter()
        .filter(|(n, _)| n == "x-seen")
        .map(|(_, v)| v.as_str())
        .collect();
    assert_eq!(seen, ["first", "second"]);
    let request = headers.iter().find(|(n, _)| n == "x-request").unwrap();
    assert_eq!(request.1, "put https example.test /echo?q=1");

    // An exception is `internal-error`, and the instance serves the next exchange.
    match exchange(
        &mut store,
        &service,
        "GET",
        "http://example.test/throw",
        vec![],
    )
    .await?
    {
        Err(ErrorCode::InternalError(Some(message))) => {
            assert_eq!(message, "TypeError: no such route")
        }
        other => panic!("expected internal-error, got {other:?}"),
    }
    match exchange(
        &mut store,
        &service,
        "GET",
        "http://example.test/bad-header",
        vec![],
    )
    .await?
    {
        Err(ErrorCode::InternalError(Some(message))) => {
            assert!(
                message.starts_with("TypeError: Invalid header"),
                "{message}"
            )
        }
        other => panic!("expected internal-error, got {other:?}"),
    }
    let (status, headers, body) = exchange(
        &mut store,
        &service,
        "PURGE",
        "http://example.test/missing",
        vec![],
    )
    .await?
    .map_err(|e| anyhow!("{e:?}"))?;
    assert_eq!((status, body.len()), (404, 0));
    let request = headers.iter().find(|(n, _)| n == "x-request").unwrap();
    assert_eq!(request.1, "other:PURGE http example.test /missing");
    assert!(store.data().table.is_empty(), "host resources leaked");
    Ok(())
}
