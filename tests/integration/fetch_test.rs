//! `fetch` over `wasi:http/client`: request method, headers and body reach the server; the
//! response's status, headers and body read back through `text()`, `bytes()`, `json()` and
//! readers.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Store,
    component::{Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

use crate::{engine::async_engine, http_fixture};

const WIT: &str = r#"
package test:fetch;
world fetch {
  include wasi:cli/imports@0.3.0;
  import wasi:http/client@0.3.0;
  export run: async func(url: string) -> string;
}
"#;

const SOURCE: &str = r#"
export async function run(url: string): Promise<string> {
  const posted = await fetch(url + '/echo?x=1', {
    method: 'post',
    headers: { 'X-Token': ' abc ' },
    body: 'héllo',
  });
  const text = await posted.text();
  let used = false;
  try { await posted.bytes(); } catch (e) { if (e instanceof TypeError) used = true; }
  const got = await fetch(url + '/bytes', { headers: {} });
  const bytes = await got.bytes();
  const streamed = await fetch(url + '/bytes', { headers: {} });
  const body = streamed.body;
  let count = 0;
  if (body !== null) {
    const reader = body.getReader();
    let result = await reader.read();
    while (!result.done) {
      const chunk = result.value;
      if (chunk !== undefined) count += chunk.length;
      result = await reader.read();
    }
    reader.releaseLock();
  }
  const missing = await fetch(url + '/missing', { headers: {} });
  return text + '|' + posted.status + '|' + used + '|' + posted.headers.get('x-reply') + '|' +
    bytes.length + '|' + count + '|' + streamed.bodyUsed + '|' + missing.status + '|' + missing.ok;
}
"#;

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

/// Compile `source`, call its `run(url)` export against `server` and return the result.
async fn run(source: &str, server: &http_fixture::HttpFixture) -> Result<String> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("fetch"))?;
    let compiled = compile_for_world(source, "fetch.ts", &resolve, world, true)?;
    let engine = async_engine()?;
    let component = wasmtime::component::Component::new(&engine, compiled.stripped.unwrap())?;
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
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let url = format!("http://{}", server.address);
    let (text,) = run.call_async(&mut store, (&url,)).await?;
    assert!(store.data().table.is_empty(), "host resources leaked");
    Ok(text)
}

#[tokio::test(flavor = "current_thread")]
async fn fetch_sends_requests_and_reads_responses() -> Result<()> {
    let server = http_fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/echo?x=1" => http_fixture::Reply::WithHeaders(
            201,
            vec![("x-reply".into(), "yes".into())],
            format!(
                "{} {} {}",
                request.method,
                request
                    .headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("x-token"))
                    .map_or("", |(_, v)| v.as_str()),
                String::from_utf8_lossy(&request.body)
            ),
        ),
        "/bytes" => http_fixture::Reply::Bytes(200, (0..20_000u32).map(|i| i as u8).collect()),
        _ => http_fixture::Reply::Body(404, "no".into()),
    });
    let text = run(SOURCE, &server).await?;
    assert_eq!(
        text,
        "POST abc héllo|201|true|yes|20000|20000|true|404|false"
    );
    Ok(())
}

/// `examples/merge_docs.ts` with the server's address: concurrent requests, `json()` and an
/// object spread of the parsed documents. A body that is not JSON rejects.
const MERGE: &str = r#"
export async function run(url: string): Promise<string> {
  const [res1, res2] = await Promise.all([fetch(url + "/doc1.json"), fetch(url + "/doc2.json")]);
  const doc1 = await res1.json();
  const doc2 = await res2.json();
  let rejected = false;
  try { await (await fetch(url + "/broken")).json(); } catch (e) { rejected = true; }
  return JSON.stringify({ ...doc1, ...doc2 }) + "|" + rejected;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn fetched_json_documents_merge_like_node() -> Result<()> {
    let server = http_fixture::HttpFixture::new(|request| match request.target.as_str() {
        "/doc1.json" => http_fixture::Reply::Body(200, r#"{"b":1,"2":"x","a":[1,2]}"#.into()),
        "/doc2.json" => {
            http_fixture::Reply::Bytes(200, "\u{feff}{\"a\":null,\"1\":true,\"c\":\"é\"}".into())
        }
        _ => http_fixture::Reply::Body(200, "{\"a\":".into()),
    });
    // Recorded from Node 22.
    assert_eq!(
        run(MERGE, &server).await?,
        r#"{"1":true,"2":"x","b":1,"a":null,"c":"é"}|true"#
    );
    Ok(())
}
