//! `fetch` over `wasi:http/client`: request method, headers and body reach the server; the
//! response's status, headers and body read back through `text()`, `bytes()` and readers.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, Store,
    component::{Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

#[allow(dead_code)]
#[path = "support/http_fixture.rs"]
mod http_fixture;

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
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("fetch"))?;
    let compiled = compile_for_world(SOURCE, "fetch.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
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
    assert_eq!(
        text,
        "POST abc héllo|201|true|yes|20000|20000|true|404|false"
    );
    assert!(store.data().table.is_empty(), "host resources leaked");
    Ok(())
}
