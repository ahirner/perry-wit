use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;
use wasmtime::StoreContextMut;
use wasmtime::component::{
    FutureReader, Linker, Resource, Source, StreamConsumer, StreamReader, StreamResult,
};
use wasmtime_wasi::filesystem::Descriptor;
use wasmtime_wasi::p3::bindings::filesystem::types::ErrorCode;
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

use super::{Host, instantiate_with};

type WriteArguments = (Resource<Descriptor>, StreamReader<u8>, u64);

const WRITE: &str = r#"
import {writeFile} from "fs/promises";
async function save(path: string, text: string): Promise<number> {
    try { (await writeFile(path, text)); return 0; }
    catch (error) { return error; }
}
export async function run(): Promise<number> {
    const pending = save("/sandbox/" + "file", "é" + "😀\0");
    let index = 0;
    while (index < 2000) { const scratch = new Uint8Array(1024); index = index + 1; }
    return await pending;
}"#;

#[tokio::test(flavor = "current_thread")]
async fn filesystem_future_errors_survive_sibling_collection_after_successful_transfer()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let control = CompletionControl::default();
    let (mut store, instance) =
        instantiate_with(WRITE, context, |linker| control.configure(linker)).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..30 {
        let mut invocation = Box::pin(run.call_async(&mut store, ()));
        tokio::select! {
            result = &mut invocation => panic!("returned before filesystem completion: {result:?}"),
            () = control.transferred.notified() => {}
        }
        assert!(
            timeout(Duration::from_millis(1), &mut invocation)
                .await
                .is_err()
        );
        control.finish.notify_one();
        assert_eq!(timeout(Duration::from_secs(5), invocation).await??.0, 37.0);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(
        *control.bytes.lock().unwrap(),
        "é😀\0".repeat(30).as_bytes()
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn disposal_closes_pending_filesystem_owners_without_stale_guest_cleanup() -> Result<()> {
    let source = r#"
    import {writeFile} from "fs/promises";
    export async function run(): Promise<Result<number, number>> {
        try { (await writeFile("/sandbox/file", "pending")); return 1; }
        finally { (await writeFile("/sandbox/finally", "unexpected")); }
    }"#;
    let directory = tempfile::tempdir()?;
    let context = WasiCtxBuilder::new()
        .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
        .build();
    let control = CompletionControl::default();
    let (mut store, instance) =
        instantiate_with(source, context, |linker| control.configure(linker)).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    let mut invocation = Box::pin(run.call_async(&mut store, ()));
    tokio::select! {
        result = &mut invocation => panic!("returned before filesystem completion: {result:?}"),
        () = control.transferred.notified() => {}
    }
    drop(invocation);
    assert!(!store.data().table.is_empty());
    drop(store);
    assert!(!directory.path().join("finally").exists());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn incomplete_transfers_fail_and_host_traps_bypass_language_cleanup() -> Result<()> {
    use crate::output_capture::MemoryOutput;

    let source = r#"
    import {writeFile} from "fs/promises";
    export async function run(): Promise<number> {
        try { (await writeFile("/sandbox/file", "must be transferred")); return 0; }
        catch (error) { return error; }
        finally { console.log("finished"); }
    }"#;
    for trap in [false, true] {
        let directory = tempfile::tempdir()?;
        let output = MemoryOutput::new(4096);
        let context = WasiCtxBuilder::new()
            .preopened_dir(directory.path(), "/sandbox", FsPerms::ReadWrite)?
            .stdout(output.clone())
            .build();
        let (mut store, instance) = instantiate_with(source, context, |linker| {
            linker.allow_shadowing(true);
            linker.instance("wasi:filesystem/types@0.3.0")?.func_wrap(
                "[method]descriptor.write-via-stream",
                move |mut store: StoreContextMut<'_, Host>,
                      (descriptor, mut input, _): WriteArguments| {
                    store.data().table.get(&descriptor)?;
                    input.close(&mut store)?;
                    wasmtime::error::ensure!(!trap, "controlled filesystem trap");
                    Ok((FutureReader::new(&mut store, async {
                        wasmtime::error::Ok(Ok::<(), ErrorCode>(()))
                    })?,))
                },
            )?;
            Ok(())
        })
        .await?;
        let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
        if trap {
            let error = run.call_async(&mut store, ()).await.unwrap_err();
            assert!(format!("{error:#}").contains("controlled filesystem trap"));
            assert!(output.contents().is_empty());
        } else {
            for _ in 0..30 {
                assert_eq!(run.call_async(&mut store, ()).await?.0, 32.0);
                store.assert_concurrent_state_empty();
                assert!(store.data().table.is_empty());
            }
            assert_eq!(
                output.contents().as_ref(),
                "finished\n".repeat(30).as_bytes()
            );
        }
        drop(store);
    }
    Ok(())
}

#[derive(Default)]
struct CompletionControl {
    bytes: Arc<Mutex<Vec<u8>>>,
    transferred: Arc<Notify>,
    finish: Arc<Notify>,
}

impl CompletionControl {
    fn configure(&self, linker: &mut Linker<Host>) -> Result<()> {
        let captured = self.bytes.clone();
        let transferred = self.transferred.clone();
        let finish = self.finish.clone();
        linker.allow_shadowing(true);
        linker.instance("wasi:filesystem/types@0.3.0")?.func_wrap(
            "[method]descriptor.write-via-stream",
            move |mut store: StoreContextMut<'_, Host>,
                  (descriptor, input, offset): WriteArguments| {
                store.data().table.get(&descriptor)?;
                assert_eq!(offset, 0);
                let (closed, receiver) = oneshot::channel();
                input.pipe(
                    &mut store,
                    Capture {
                        bytes: captured.clone(),
                        closed: Some(closed),
                    },
                )?;
                let transferred = transferred.clone();
                let finish = finish.clone();
                Ok((FutureReader::new(&mut store, async move {
                    receiver.await?;
                    transferred.notify_one();
                    finish.notified().await;
                    wasmtime::error::Ok(Err::<(), _>(ErrorCode::Other(Some("é😀".repeat(300)))))
                })?,))
            },
        )?;
        Ok(())
    }
}

struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    closed: Option<oneshot::Sender<()>>,
}
impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(closed) = self.closed.take() {
            let _ = closed.send(());
        }
    }
}
impl StreamConsumer<Host> for Capture {
    type Item = u8;
    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        store: StoreContextMut<'_, Host>,
        source: Source<u8>,
        _: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut source = source.as_direct(store);
        let bytes = source.remaining();
        let count = bytes.len().min(3);
        self.bytes
            .lock()
            .unwrap()
            .extend_from_slice(&bytes[..count]);
        source.mark_read(count);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}
