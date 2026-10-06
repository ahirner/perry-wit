use anyhow::Result;
use bytes::Bytes;
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Full};
use perry_wit::waffle_backend::{HttpHandlerOptions, WaffleCompileOptions, compile_http_handler};
use std::{
    collections::VecDeque,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use wasmtime::component::Accessor;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{AsContextMut, Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};
use wasmtime_wasi_http::p3::{Request, bindings::Service};
use wasmtime_wasi_http::{Error, p3::bindings::http::types::ErrorCode};
use wasmtime_wasi_http::{WasiHttpCtx, WasiHttpCtxView, WasiHttpView};

struct Host {
    http: WasiHttpCtx,
    wasi: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
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

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

async fn drain(accessor: &Accessor<Host>) {
    while accessor.with(|mut store| store.as_context_mut().concurrent_state_table_size()) != 0 {
        tokio::task::yield_now().await;
    }
}

async fn instantiate(source: &str, limit: u32) -> Result<(Store<Host>, Service)> {
    let directory = tempfile::tempdir()?;
    perry_wit::generate_sdk_files(&perry_wit::SdkOptions {
        wit_dir: std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/waffle_backend/http/handler"
        )),
        world: Some("handler".into()),
        out_dir: directory.path().to_owned(),
        project_root: Some(directory.path().to_owned()),
        entry: "handler.ts".into(),
        initialize_tsconfig: false,
    })?;
    std::fs::write(directory.path().join("handler.ts"), source)?;
    let checked = std::process::Command::new("tsc")
        .current_dir(directory.path())
        .args([
            "--ignoreConfig",
            "--noEmit",
            "--strict",
            "--target",
            "ES2022",
            "--module",
            "esnext",
            "handler.ts",
            "p3.d.ts",
        ])
        .output()?;
    anyhow::ensure!(
        checked.status.success(),
        "{}{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let compiled = compile_http_handler(
        source,
        "handler.ts",
        &WaffleCompileOptions::default(),
        HttpHandlerOptions {
            max_request_bytes: limit,
            max_response_bytes: limit,
        },
    )?;
    let mut config = Config::new();
    config
        .wasm_component_model_async(true)
        .wasm_component_model_more_async_builtins(true)
        .wasm_component_model_threading(true)
        .wasm_component_model_async_stackful(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            http: WasiHttpCtx::new(),
            wasi: WasiCtx::default(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(1024 * 1024).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = Service::instantiate_async(&mut store, &component, &linker).await?;
    Ok((store, instance))
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_handler_roundtrips_binary_and_duplicate_headers() -> Result<()> {
    let source = r#"
        import type {Request, Response} from "perry:http-handler/types";
        export function handle(request:Request):Response {
            if(request.method.tag!=='put') {throw 1;}
            if(request.pathWithQuery!=='/echo?q=1') {throw 2;}
            return {status:201,headers:request.headers,body:request.body};
        }
    "#;
    let (mut store, service) = instantiate(source, 65536).await?;
    for length in [0, 1, 65536].into_iter().cycle().take(90) {
        let payload: Vec<u8> = (0..length).map(|index| index as u8).collect();
        let request = http::Request::builder()
            .method("PUT")
            .uri("https://example.test/echo?q=1")
            .header("x-value", "first")
            .header("x-value", "second")
            .body(Full::new(Bytes::from(payload.clone())))?;
        let (request, completed) = Request::from_http(wasmtime_wasi_http::default_hooks(), request);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            store.run_concurrent(async |accessor| -> Result<()> {
                let response = service
                    .handle(accessor, request)
                    .await?
                    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
                let response =
                    accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
                assert_eq!(response.status(), 201);
                assert_eq!(
                    response
                        .headers()
                        .get_all("x-value")
                        .iter()
                        .map(|v| v.to_str().unwrap())
                        .collect::<Vec<_>>(),
                    ["first", "second"]
                );
                assert_eq!(response.into_body().collect().await?.to_bytes(), payload);
                completed.await?;
                drain(accessor).await;
                Ok(())
            }),
        )
        .await???;
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}

const ECHO: &str = r#"
    import type {Request, Response} from "perry:http-handler/types";
    export function handle(request:Request):Response {
        return {status:200,headers:request.headers,body:request.body};
    }
"#;

type Reply = std::result::Result<(u16, http::HeaderMap, Bytes), ErrorCode>;

async fn exchange<B>(
    store: &mut Store<Host>,
    service: &Service,
    body: B,
) -> Result<(Reply, std::result::Result<(), Error>)>
where
    B: Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Error>,
{
    let request = http::Request::builder()
        .uri("https://example.test/")
        .body(body)?;
    let (request, completed) = Request::from_http(wasmtime_wasi_http::default_hooks(), request);
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        store.run_concurrent(async |accessor| -> Result<_> {
            let response = match service.handle(accessor, request).await? {
                Ok(response) => {
                    let response = accessor
                        .with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
                    let (parts, body) = response.into_parts();
                    Ok((
                        parts.status.as_u16(),
                        parts.headers,
                        body.collect().await?.to_bytes(),
                    ))
                }
                Err(error) => Err(error),
            };
            let completed = completed.await;
            drain(accessor).await;
            Ok((response, completed))
        }),
    )
    .await???;
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(result)
}

struct DelayedFrames {
    frames: VecDeque<std::result::Result<Frame<Bytes>, Error>>,
    delay: Pin<Box<tokio::time::Sleep>>,
    drops: Arc<AtomicUsize>,
}
impl Body for DelayedFrames {
    type Data = Bytes;
    type Error = Error;
    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<Frame<Bytes>, Error>>> {
        if self.delay.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        self.delay
            .as_mut()
            .reset(tokio::time::Instant::now() + Duration::from_millis(1));
        Poll::Ready(self.frames.pop_front())
    }
}
impl Drop for DelayedFrames {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn request_overflow_and_producer_failure_release_resources() -> Result<()> {
    let (mut store, service) = instantiate(ECHO, 4).await?;
    for _ in 0..40 {
        let (reply, completed) = exchange(
            &mut store,
            &service,
            Full::new(Bytes::from_static(b"12345")),
        )
        .await?;
        assert!(matches!(reply, Err(ErrorCode::HttpRequestBodySize(None))));
        assert!(matches!(completed, Err(Error::HttpRequestBodySize(None))));
        let drops = Arc::new(AtomicUsize::new(0));
        let body = DelayedFrames {
            frames: [
                Ok(Frame::data(Bytes::from_static(b"ab"))),
                Err(Error::ConnectionTerminated),
            ]
            .into(),
            delay: Box::pin(tokio::time::sleep(Duration::from_millis(1))),
            drops: drops.clone(),
        };
        let (reply, completed) = exchange(&mut store, &service, body).await?;
        assert!(matches!(reply, Err(ErrorCode::InternalError(None))));
        assert!(matches!(completed, Err(Error::InternalError(None))));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let (reply, completed) =
            exchange(&mut store, &service, Full::new(Bytes::from_static(b"1234"))).await?;
        assert_eq!(reply?.2, b"1234"[..]);
        completed?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_responses_and_response_limit_return_domain_errors() -> Result<()> {
    for (status, body, headers, overflow) in [
        (200, "new Uint8Array(5)", "[]", true),
        (199, "request.body", "[]", false),
        (600, "request.body", "[]", false),
        (204, "new Uint8Array(2)", "[]", false),
        (205, "new Uint8Array(1)", "[]", false),
        (304, "new Uint8Array(2)", "[]", false),
        (
            200,
            "request.body",
            r#"[["bad header",new Uint8Array(0)]]"#,
            false,
        ),
    ] {
        let source = ECHO
            .replace("status:200", &format!("status:{status}"))
            .replace("body:request.body", &format!("body:{body}"))
            .replace("headers:request.headers", &format!("headers:{headers}"));
        let (mut store, service) = instantiate(&source, 4).await?;
        for _ in 0..20 {
            let (reply, completed) =
                exchange(&mut store, &service, Full::new(Bytes::new())).await?;
            if overflow {
                assert!(matches!(reply, Err(ErrorCode::HttpResponseBodySize(None))));
            } else {
                assert!(matches!(reply, Err(ErrorCode::InternalError(None))));
            }
            completed?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_handler_retains_text_bytes_and_metadata_through_delays() -> Result<()> {
    let source = r#"
        import type {Request, Response} from "perry:http-handler/types";
        import {setTimeout as waitFor} from 'node:timers/promises';
        export async function handle(request:Request):Promise<Response> {
            const text=new TextDecoder('utf-8',{fatal:true}).decode(request.body);
            const first=waitFor(1);
            const second=waitFor(2);
            await Promise.all([first,second]);
            const scheme=request.scheme;
            if(scheme!==undefined && scheme!==null) { if(scheme.tag!=='https') {throw 1;} } else {throw 3;}
            if(request.authority!=='example.test') {throw 2;}
            const value=JSON.parse(text);
            for(let index=0;index<400;index++) {JSON.parse(text);}
            if(JSON.stringify(value)!==text) {throw 4;}
            return {status:202,headers:request.headers,body:request.body};
        }
    "#;
    let (mut store, service) = instantiate(source, 65536).await?;
    for _ in 0..30 {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut trailers = http::HeaderMap::new();
        trailers.insert("x-finished", http::HeaderValue::from_static("yes"));
        let body = DelayedFrames {
            frames: [
                Ok(Frame::data(Bytes::from_static(b"{\"label\":"))),
                Ok(Frame::data(Bytes::from_static("\"snow 雪\"}".as_bytes()))),
                Ok(Frame::trailers(trailers)),
            ]
            .into(),
            delay: Box::pin(tokio::time::sleep(Duration::from_millis(1))),
            drops: drops.clone(),
        };
        let (reply, completed) = exchange(&mut store, &service, body).await?;
        let (status, _, body) = reply?;
        assert_eq!(status, 202);
        assert_eq!(std::str::from_utf8(&body)?, "{\"label\":\"snow 雪\"}");
        completed?;
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn response_abandonment_and_consumer_failure_settle_before_reuse() -> Result<()> {
    let (mut store, service) = instantiate(ECHO, 131072).await?;
    for abandon in [false, true].into_iter().cycle().take(60) {
        let request = http::Request::new(Full::new(Bytes::from(vec![7; 131072])));
        let (request, completed) = Request::from_http(wasmtime_wasi_http::default_hooks(), request);
        tokio::time::timeout(
            Duration::from_secs(10),
            store.run_concurrent(async |accessor| -> Result<()> {
                let response = service.handle(accessor, request).await??;
                let response = accessor.with(|mut store| {
                    response.into_http(&mut store, async { Err(Error::ConnectionTerminated) })
                })?;
                if abandon {
                    drop(response);
                } else {
                    assert_eq!(
                        response.into_body().collect().await?.to_bytes().len(),
                        131072
                    );
                }
                completed.await?;
                drain(accessor).await;
                Ok(())
            }),
        )
        .await???;
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    let (reply, completed) = exchange(
        &mut store,
        &service,
        Full::new(Bytes::from_static(b"usable")),
    )
    .await?;
    assert_eq!(reply?.2, b"usable"[..]);
    completed?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn overlapping_calls_wait_for_previous_body_completion() -> Result<()> {
    let (mut store, service) = instantiate(ECHO, 65536).await?;
    let (first, first_done) = Request::from_http(
        wasmtime_wasi_http::default_hooks(),
        http::Request::new(Full::new(Bytes::from(vec![1; 65536]))),
    );
    let (second, second_done) = Request::from_http(
        wasmtime_wasi_http::default_hooks(),
        http::Request::new(Full::new(Bytes::from(vec![2; 65536]))),
    );
    tokio::time::timeout(
        Duration::from_secs(10),
        store.run_concurrent(async |accessor| -> Result<()> {
            let response = service.handle(accessor, first).await??;
            let (acknowledge, consumed) = tokio::sync::oneshot::channel();
            let response = accessor.with(|mut store| {
                response.into_http(&mut store, async move {
                    consumed.await.map_err(|_| Error::ConnectionTerminated)
                })
            })?;
            let mut second = Box::pin(service.handle(accessor, second));
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut second)
                    .await
                    .is_err()
            );
            assert_eq!(
                response.into_body().collect().await?.to_bytes(),
                vec![1; 65536]
            );
            acknowledge.send(()).unwrap();
            let response = second.await??;
            let response =
                accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
            assert_eq!(
                response.into_body().collect().await?.to_bytes(),
                vec![2; 65536]
            );
            first_done.await?;
            second_done.await?;
            drain(accessor).await;
            Ok(())
        }),
    )
    .await???;
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_input_is_released_when_store_is_disposed() -> Result<()> {
    let (mut store, service) = instantiate(ECHO, 65536).await?;
    let drops = Arc::new(AtomicUsize::new(0));
    let body = DelayedFrames {
        frames: [Ok(Frame::data(Bytes::from_static(b"waiting")))].into(),
        delay: Box::pin(tokio::time::sleep(Duration::from_secs(3600))),
        drops: drops.clone(),
    };
    let (request, completed) = Request::from_http(
        wasmtime_wasi_http::default_hooks(),
        http::Request::new(body),
    );
    assert!(
        tokio::time::timeout(
            Duration::from_millis(30),
            store.run_concurrent(async |accessor| service.handle(accessor, request).await)
        )
        .await
        .is_err()
    );
    drop(store);
    drop(completed);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_source_data_traps_and_requires_store_disposal() -> Result<()> {
    let source=ECHO.replace("return {status:200", "JSON.parse(new TextDecoder('utf-8',{fatal:true}).decode(request.body)); return {status:200");
    for input in [Bytes::from_static(b"{bad"), Bytes::from_static(&[0xff])] {
        let (mut store, service) = instantiate(&source, 64).await?;
        let (request, completed) = Request::from_http(
            wasmtime_wasi_http::default_hooks(),
            http::Request::new(Full::new(input)),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            store.run_concurrent(async |accessor| service.handle(accessor, request).await),
        )
        .await?;
        assert!(matches!(result, Err(_) | Ok(Err(_))));
        drop(store);
        drop(completed);
    }
    Ok(())
}

#[test]
fn handler_imports_only_required_capabilities_and_checks_source_types() -> Result<()> {
    let options = WaffleCompileOptions::default();
    let limits = HttpHandlerOptions {
        max_request_bytes: 65536,
        max_response_bytes: 65536,
    };
    let component = compile_http_handler(ECHO, "handler.ts", &options, limits)?
        .component_wat
        .unwrap();
    assert!(component.contains("wasi:http/handler@0.3.0"));
    for unused in [
        "wasi:http/client",
        "wasi:filesystem/",
        "wasi:clocks/",
        "wasi:random/",
    ] {
        assert!(!component.contains(unused));
    }
    for body in ["await Promise.resolve(1);", "await Promise.reject(1);"] {
        let source = ECHO
            .replace("function handle", "async function handle")
            .replace("):Response", "):Promise<Response>")
            .replace("return {status:200", &format!("{body} return {{status:200"));
        assert!(
            format!(
                "{:#}",
                compile_http_handler(&source, "invalid.ts", &options, limits).unwrap_err()
            )
            .contains("D5")
        );
    }
    let source = ECHO.replace("body:request.body", "body:'text'");
    assert!(compile_http_handler(&source, "invalid.ts", &options, limits).is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn custom_method_scheme_and_absent_coordinates_preserve_variants() -> Result<()> {
    let source = r#"
        import type {Request,Response} from 'perry:http-handler/types';
        export function handle(request:Request):Response {
            const method=request.method;
            if(method.tag!=='other') {throw 1;}
            if(method.tag==='other') {if(method.val!=='REPORT') {throw 2;}}
            const scheme=request.scheme;
            if(scheme!==undefined && scheme!==null) {
                if(scheme.tag!=='other') {throw 3;}
                if(scheme.tag==='other') {if(scheme.val!=='custom') {throw 4;}}
                if(request.authority!=='example.test:8443') {throw 5;}
            } else {if(request.authority!==undefined) {throw 6;}}
            if(request.pathWithQuery!=='/a?b=c') {throw 7;}
            return {status:204,headers:request.headers,body:request.body};
        }
    "#;
    let (mut store, service) = instantiate(source, 0).await?;
    for uri in ["custom://example.test:8443/a?b=c", "/a?b=c"] {
        let request = http::Request::builder()
            .method("REPORT")
            .uri(uri)
            .body(Full::new(Bytes::new()))?;
        let (request, completed) = Request::from_http(wasmtime_wasi_http::default_hooks(), request);
        tokio::time::timeout(
            Duration::from_secs(10),
            store.run_concurrent(async |accessor| -> Result<()> {
                let response = service.handle(accessor, request).await??;
                let response =
                    accessor.with(|mut store| response.into_http(&mut store, async { Ok(()) }))?;
                assert_eq!(response.status(), 204);
                assert!(response.into_body().collect().await?.to_bytes().is_empty());
                completed.await?;
                drain(accessor).await;
                Ok(())
            }),
        )
        .await???;
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}
