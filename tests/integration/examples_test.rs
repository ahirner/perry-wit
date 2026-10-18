//! Execute checked-in sources against their checked-in WIT contracts.
use anyhow::Result;
use http_body_util::{BodyExt, Full};
use std::{
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wasmtime::error::Context as _;
use wasmtime::{
    Engine, Store,
    component::{Component, ComponentType, Lift, Linker},
};
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

use crate::engine::async_engine;
use crate::wasi_host::{self, Host};

#[derive(Default)]
struct Documents {
    started: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    invalid: bool,
    http_error: bool,
}

struct PendingRequest(Arc<AtomicUsize>);
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl WasiHttpHooks for Documents {
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
        if request.uri().path() == "/pending" {
            self.started.fetch_add(1, Ordering::SeqCst);
            let pending = PendingRequest(self.cancelled.clone());
            return Box::new(async move {
                let _pending = pending;
                std::future::pending().await
            });
        }
        let body = match request.uri().path() {
            "/doc1.json" if self.invalid => "invalid JSON",
            "/doc1.json" => include_str!("../../examples/merge_docs/doc1.json"),
            "/doc2.json" => include_str!("../../examples/merge_docs/doc2.json"),
            path => panic!("unexpected example request {path}"),
        };
        let status = if self.http_error { 503 } else { 200 };
        let started = self.started.clone();
        started.fetch_add(1, Ordering::SeqCst);
        Box::new(async move {
            // Both requests must start before either completes; no guest thread feature.
            while started.load(Ordering::SeqCst) < 2 {
                tokio::task::yield_now().await;
            }
            let body = Full::new(bytes::Bytes::from_static(body.as_bytes()))
                .map_err(|never| match never {})
                .boxed_unsync();
            Ok((
                http::Response::builder().status(status).body(body).unwrap(),
                Box::new(async { Ok(()) }) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn detached_fetch_is_cancelled_before_task_boundary_failure() -> Result<()> {
    let (resolve, package) = perry_wit::component::wit::resolve_source(
        "detached.wit",
        "package test:detached; world run {import wasi:http/client@0.3.0; export run:async func()->u32;}",
    )?;
    let world = resolve.select_world(&[package], Some("run"))?;
    let compiled = perry_wit::compile_for_world(
        "export async function run():Promise<number> {fetch('https://example.test/pending');return 7;}",
        "detached.ts",
        &resolve,
        world,
        true,
    )?;
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &compiled.component.unwrap().stripped,
        Host::default().with_hooks(Documents::default()),
    )
    .await?;
    let run = instance.get_typed_func::<(), (u32,)>(&mut store, "run")?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(&mut store, ()),
    )
    .await?;
    assert!(
        result.is_err(),
        "unresolved detached fetch must fail the task"
    );
    assert_eq!(store.data().hooks.started.load(Ordering::SeqCst), 1);
    assert_eq!(store.data().hooks.cancelled.load(Ordering::SeqCst), 1);
    assert!(
        store.data().table.is_empty(),
        "cancelled request leaked host handles"
    );
    Ok(())
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
struct MergeError {
    name: String,
    message: String,
}

#[tokio::test(flavor = "current_thread")]
async fn checked_in_merge_docs_overlaps_fetches_and_returns_declared_errors() -> Result<()> {
    let (resolve, package) =
        perry_wit::component::wit::resolve_wit(std::path::Path::new("examples/merge_docs"))?;
    let world = resolve.select_world(&[package], Some("merge-docs"))?;
    let compiled = perry_wit::compile_for_world(
        include_str!("../../examples/merge_docs/index.ts"),
        "examples/merge_docs/index.ts",
        &resolve,
        world,
        true,
    )?;
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &compiled.component.unwrap().stripped,
        Host::default().with_hooks(Documents::default()),
    )
    .await?;
    let run = instance
        .get_typed_func::<(&str, &str), (Result<String, MergeError>,)>(&mut store, "merge-docs")?;
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(
            &mut store,
            (
                "https://example.test/doc1.json",
                "https://example.test/doc2.json",
            ),
        ),
    )
    .await?
    .context("successful document merge")?;
    let actual: serde_json::Value = serde_json::from_str(&result.expect("merged documents"))?;
    let mut expected: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("../../examples/merge_docs/doc1.json"))?;
    expected.extend(serde_json::from_str::<
        serde_json::Map<String, serde_json::Value>,
    >(include_str!("../../examples/merge_docs/doc2.json"))?);
    assert_eq!(actual, serde_json::Value::Object(expected));
    assert_eq!(store.data().hooks.started.load(Ordering::SeqCst), 2);
    assert!(
        store.data().table.is_empty(),
        "successful merge leaked host resources"
    );
    store.data_mut().hooks.invalid = true;
    store.data().hooks.started.store(0, Ordering::SeqCst);
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(
            &mut store,
            (
                "https://example.test/doc1.json",
                "https://example.test/doc2.json",
            ),
        ),
    )
    .await?
    .context("malformed document merge")?;
    let error = result.expect_err("invalid document must become a WIT error");
    assert_eq!(error.name, "SyntaxError");
    assert!(error.message.contains("JSON"), "{}", error.message);
    assert!(
        store.data().table.is_empty(),
        "failed merge leaked host resources"
    );
    store.data_mut().hooks.invalid = false;
    store.data_mut().hooks.http_error = true;
    store.data().hooks.started.store(0, Ordering::SeqCst);
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(
            &mut store,
            (
                "https://example.test/doc1.json",
                "https://example.test/doc2.json",
            ),
        ),
    )
    .await?
    .context("HTTP failure merge")?;
    assert!(result.unwrap_err().message.contains("503"));
    assert!(store.data().table.is_empty());
    store.data().hooks.started.store(0, Ordering::SeqCst);
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(&mut store, ("not a URL", "https://example.test/pending")),
    )
    .await??;
    assert_eq!(result.unwrap_err().name, "TypeError");
    assert_eq!(store.data().hooks.started.load(Ordering::SeqCst), 1);
    assert_eq!(store.data().hooks.cancelled.load(Ordering::SeqCst), 1);
    assert!(
        store.data().table.is_empty(),
        "losing fetch leaked host resources"
    );
    Ok(())
}

#[derive(ComponentType, Lift, Debug, PartialEq)]
#[component(record)]
struct Document {
    markdown: String,
    #[component(name = "code-points")]
    code_points: u32,
}

#[test]
fn checked_in_markdown_example_counts_unicode_code_points() -> Result<()> {
    let compiled = perry_wit::compile_file(
        std::path::Path::new("examples/merge_task/index.ts"),
        &perry_wit::CompileOptions {
            wit_dir: "examples/merge_task".into(),
            world: Some("merge-task".into()),
            core_only: false,
        },
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let render =
        instance.get_typed_func::<(&str, Vec<&str>), (Document,)>(&mut store, "render-markdown")?;
    for (title, paragraphs, markdown) in [
        (
            "  Field notes 🍄  ",
            vec![" First ", "  ", "Café 🙂 "],
            "# Field notes 🍄\n\nFirst\n\nCafé 🙂",
        ),
        (" ", vec![" ", ""], ""),
        ("Title", vec![], "# Title"),
        ("", vec![" e\u{301} ", "👩‍💻"], "e\u{301}\n\n👩‍💻"),
    ] {
        assert_eq!(
            render.call(&mut store, (title, paragraphs))?.0,
            Document {
                markdown: markdown.into(),
                code_points: markdown.chars().count() as u32
            }
        );
    }
    let count = instance.get_typed_func::<(&str,), (u32,)>(&mut store, "count-code-points")?;
    for (text, expected) in [("", 0), ("Café 🙂", 6), ("e\u{301}", 2), ("👩‍💻", 3)] {
        assert_eq!(count.call(&mut store, (text,))?.0, expected);
    }
    Ok(())
}

#[test]
fn starter_template_still_executes() -> Result<()> {
    let compiled = perry_wit::compile_file(
        std::path::Path::new("template/src/index.ts"),
        &perry_wit::CompileOptions {
            wit_dir: "template/wit".into(),
            world: Some("task".into()),
            core_only: false,
        },
    )?;
    let engine = Engine::default();
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
    let mut store = Store::new(&engine, ());
    let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
    let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, "run-task")?;
    assert_eq!(
        run.call(&mut store, ("hello",))?.0,
        "[task] Processed input: hello"
    );
    Ok(())
}
