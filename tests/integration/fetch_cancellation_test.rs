//! Host cancellation uses the existing HTTP hook and 03ea's component cancellation caller.
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
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

use crate::cancellation;
use crate::engine::async_engine;
use crate::wasi_host::{self, Host};

#[derive(Clone, Copy, Debug)]
enum Phase {
    Request,
    Body,
}
#[derive(Default)]
struct Observations {
    ready: AtomicBool,
    requests: AtomicUsize,
    dropped_requests: AtomicUsize,
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
            let _request = request;
            if matches!(phase, Phase::Request) {
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
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "cancel.wit",
        "package test:fetch-cancel; world run {import wasi:http/client@0.3.0;import wasi:cli/stderr@0.3.0;export run:async func();}",
    )?;
    let world = resolve.select_world(&[package], Some("run"))?;
    let compiled = perry_wit::compiler::compile_for_world_with_memory_checks(
        "export async function run():Promise<void>{const response=await fetch('https://example.test/body');try{await response.bytes();}finally{throw new Error('cancelled continuation ran');}}",
        "cancel.ts",
        &resolve,
        world,
    )?;
    let engine = async_engine()?;
    let component = Component::new(
        &engine,
        cancellation::cancellation_caller(&compiled.component.unwrap().stripped, true)?,
    )?;
    for phase in [Phase::Request, Phase::Body] {
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
