//! `fetch` over `wasi:http/client`: request method, headers and body reach the server; the
//! response's status, headers and body read back through `text()`, `bytes()`, and readers; and
//! host cancellation releases requests and partial bodies.
use crate::wasi_host::{self, Host};
use crate::world::Compiler::{MemoryChecked, Standard};
use crate::{cancellation, engine::async_engine, http_fixture};
use anyhow::Result;
use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};
use wasmtime::component::Component;
use wasmtime_wasi_http::p3::bindings::http::types::ErrorCode;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

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
    let compiled = Standard.compile(WIT, "fetch", "fetch.ts", SOURCE)?;
    let has_json_parser = crate::world::function_names(&compiled.core)?
        .into_iter()
        .any(|name| matches!(name, "json_measure" | "json_populate_measured"));
    assert!(!has_json_parser);
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &compiled.component.unwrap().stripped,
        Host::default(),
    )
    .await?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run")?;
    let url = format!("http://{}", server.address);
    let (text,) = run.call_async(&mut store, (&url,)).await?;
    assert!(store.data().table.is_empty(), "host resources leaked");
    assert_eq!(
        text,
        "POST abc héllo|201|true|yes|20000|20000|true|404|false"
    );
    Ok(())
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Request,
    Upload,
    Body,
}

#[derive(Default)]
struct Observations {
    ready: AtomicBool,
    requests: AtomicUsize,
    dropped_requests: AtomicUsize,
    uploaded_bytes: AtomicUsize,
    body_bytes: AtomicUsize,
    dropped_bodies: AtomicUsize,
}

struct RequestGuard(Arc<Observations>);
impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.dropped_requests.fetch_add(1, Ordering::SeqCst);
    }
}

struct PartialBody {
    sent: bool,
    observations: Arc<Observations>,
}

impl Body for PartialBody {
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if self.sent {
            self.observations.ready.store(true, Ordering::SeqCst);
            Poll::Pending
        } else {
            self.sent = true;
            self.observations.body_bytes.fetch_add(3, Ordering::SeqCst);
            Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"abc")))))
        }
    }
}

impl Drop for PartialBody {
    fn drop(&mut self) {
        self.observations
            .dropped_bodies
            .fetch_add(1, Ordering::SeqCst);
    }
}

struct Fixture {
    phase: Phase,
    observations: Arc<Observations>,
}

impl WasiHttpHooks for Fixture {
    fn p3_outgoing_body_chunk_size(&mut self) -> usize {
        7
    }

    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        _: Option<RequestOptions>,
        _: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), Error>> + Send>,
                    ),
                    Error,
                >,
            > + Send,
    > {
        let phase = self.phase;
        let observations = self.observations.clone();
        observations.requests.fetch_add(1, Ordering::SeqCst);
        let guard = RequestGuard(observations.clone());
        Box::new(async move {
            let _guard = guard;
            let mut request = request;
            if !matches!(phase, Phase::Body) {
                if matches!(phase, Phase::Upload) {
                    let frame = request
                        .body_mut()
                        .frame()
                        .await
                        .ok_or(ErrorCode::HttpProtocolError)??;
                    let bytes = frame
                        .into_data()
                        .map_err(|_| ErrorCode::HttpProtocolError)?;
                    observations
                        .uploaded_bytes
                        .fetch_add(bytes.len(), Ordering::SeqCst);
                }
                observations.ready.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            let body = PartialBody {
                sent: false,
                observations,
            }
            .boxed_unsync();
            Ok((
                http::Response::new(body),
                Box::new(async { Ok(()) }) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_fetches_release_requests_and_partial_bodies() -> Result<()> {
    let bytes = MemoryChecked.component(
        "package test:fetch-cancel; world run {import wasi:http/client@0.3.0;import wasi:cli/stderr@0.3.0;export run:async func();}",
        "run",
        "cancel.ts",
        "export async function run():Promise<void>{const response=await fetch('https://example.test/body',{method:'POST',body:new Uint8Array(1024)});try{await response.bytes();}finally{throw new Error('cancelled continuation ran');}}",
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, cancellation::cancellation_caller(&bytes, true)?)?;
    for phase in [Phase::Request, Phase::Upload, Phase::Body] {
        let observations = Arc::new(Observations::default());
        let mut linker = wasi_host::linker(&engine)?;
        let readiness = observations.clone();
        linker.instance("test:control/cancellation")?.func_wrap(
            "ready",
            move |_: wasmtime::StoreContextMut<'_, Host<Fixture>>, (): ()| {
                Ok((readiness.ready.load(Ordering::SeqCst),))
            },
        )?;
        let host = Host::default()
            .with_hooks(Fixture {
                phase,
                observations: observations.clone(),
            })
            .with_memory_limit(512 * 1024);
        let mut store = wasi_host::store(&engine, host);
        let instance = linker.instantiate_async(&mut store, &component).await?;
        let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
        let mut peak = 0;
        for round in 1..=16 {
            observations.ready.store(false, Ordering::SeqCst);
            assert_eq!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    run.call_async(&mut store, ())
                )
                .await??
                .0,
                4,
                "{phase:?}"
            );
            assert_eq!(observations.requests.load(Ordering::SeqCst), round);
            assert_eq!(observations.dropped_requests.load(Ordering::SeqCst), round);
            if matches!(phase, Phase::Upload) {
                assert_eq!(
                    observations.uploaded_bytes.load(Ordering::SeqCst),
                    round * 7
                );
            }
            if matches!(phase, Phase::Body) {
                assert_eq!(observations.body_bytes.load(Ordering::SeqCst), round * 3);
                assert_eq!(observations.dropped_bodies.load(Ordering::SeqCst), round);
            }
            assert!(store.data().table.is_empty(), "{phase:?}");
            store.assert_concurrent_state_empty();
            if round == 2 {
                peak = store.data().peak;
            } else if round > 2 {
                assert_eq!(store.data().peak, peak, "{phase:?}");
            }
        }
    }
    Ok(())
}
