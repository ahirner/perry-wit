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
use wasmtime::{
    Engine, Store,
    component::{Component, ComponentType, Lift, Linker, ResourceTable},
};
use wasmtime_wasi_http::{
    Error, RequestOptions, WasiBody, WasiHttpCtx, WasiHttpCtxView, WasiHttpHooks, WasiHttpView,
};

use crate::engine::async_engine;

#[derive(Default)]
struct Documents {
    started: Arc<AtomicUsize>,
    cancelled: Arc<AtomicUsize>,
    invalid: bool,
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
            "/doc1.json" => include_str!("../../examples/doc1.json"),
            "/doc2.json" => include_str!("../../examples/doc2.json"),
            path => panic!("unexpected example request {path}"),
        };
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
                http::Response::new(body),
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
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            documents: Documents::default(),
        },
    );
    let instance = linker.instantiate_async(&mut store, &component).await?;
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
    assert_eq!(store.data().documents.started.load(Ordering::SeqCst), 1);
    assert_eq!(store.data().documents.cancelled.load(Ordering::SeqCst), 1);
    assert!(
        store.data().table.is_empty(),
        "cancelled request leaked host handles"
    );
    Ok(())
}

struct Host {
    http: WasiHttpCtx,
    table: ResourceTable,
    documents: Documents,
}
impl WasiHttpView for Host {
    fn http(&mut self) -> WasiHttpCtxView<'_> {
        WasiHttpCtxView {
            ctx: &mut self.http,
            table: &mut self.table,
            hooks: &mut self.documents,
        }
    }
}

#[derive(ComponentType, Lift, Debug)]
#[component(record)]
struct MergeError {
    name: String,
    message: String,
}

#[tokio::test(flavor = "current_thread")]
async fn checked_in_merge_docs_overlaps_fetches_and_returns_declared_errors() -> Result<()> {
    let (resolve, package) = perry_wit::component::wit::resolve_wit(std::path::Path::new("wit"))?;
    let world = resolve.select_world(&[package], Some("merge-docs"))?;
    let compiled = perry_wit::compile_for_world(
        include_str!("../../examples/merge_docs.ts"),
        "examples/merge_docs.ts",
        &resolve,
        world,
        true,
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi_http::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            http: WasiHttpCtx::new(),
            table: ResourceTable::new(),
            documents: Documents::default(),
        },
    );
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run =
        instance.get_typed_func::<(), (Result<String, MergeError>,)>(&mut store, "merge-docs")?;
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(&mut store, ()),
    )
    .await??;
    let actual: serde_json::Value = serde_json::from_str(&result.expect("merged documents"))?;
    let mut expected: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("../../examples/doc1.json"))?;
    expected.extend(serde_json::from_str::<
        serde_json::Map<String, serde_json::Value>,
    >(include_str!("../../examples/doc2.json"))?);
    assert_eq!(actual, serde_json::Value::Object(expected));
    assert_eq!(store.data().documents.started.load(Ordering::SeqCst), 2);
    assert!(
        store.data().table.is_empty(),
        "successful merge leaked host resources"
    );
    store.data_mut().documents.invalid = true;
    store.data().documents.started.store(0, Ordering::SeqCst);
    let (result,) = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run.call_async(&mut store, ()),
    )
    .await??;
    let error = result.expect_err("invalid document must become a WIT error");
    assert_eq!(error.name, "Error");
    assert!(error.message.contains("JSON"), "{}", error.message);
    assert!(
        store.data().table.is_empty(),
        "failed merge leaked host resources"
    );
    Ok(())
}

#[test]
fn checked_in_task_examples_execute_with_their_worlds() -> Result<()> {
    for (file, directory, world, export, expected) in [
        (
            "examples/merge_task.ts",
            "wit",
            "task-runner",
            "run-task",
            "TASK_PROCESSED: hello",
        ),
        (
            "examples/merge_task.ts",
            "wit",
            "merge-task",
            "merge-task",
            "MERGED_DOCUMENT: hello",
        ),
        (
            "template/src/index.ts",
            "template/wit",
            "task",
            "run-task",
            "[task] Processed input: hello",
        ),
    ] {
        let compiled = perry_wit::compiler::compile_file(
            std::path::Path::new(file),
            &perry_wit::compiler::CompileOptions {
                wit_dir: directory.into(),
                world: Some(world.into()),
                core_only: false,
            },
        )?;
        let engine = Engine::default();
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate(&mut store, &component)?;
        let run = instance.get_typed_func::<(&str,), (String,)>(&mut store, export)?;
        assert_eq!(
            run.call(&mut store, ("hello",))?.0,
            expected,
            "{file} world={world}"
        );
    }
    Ok(())
}
