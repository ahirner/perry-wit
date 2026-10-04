use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use tokio::sync::{Notify, oneshot};
use tokio::time::timeout;
use wasmtime::component::{
    Component, FutureReader, Linker, Source, StreamConsumer, StreamReader, StreamResult,
};
use wasmtime::{Config, Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::p3::bindings::cli::types::ErrorCode;

#[tokio::test(flavor = "current_thread")]
async fn successful_transfer_still_awaits_the_separate_capability_outcome() -> Result<()> {
    let source = r#"
    import {writeStdout} from "perry:stdio";
    export async function run(): Promise<number> {
        try { await writeStdout(new Uint8Array([0,128,255])); return 0; }
        catch (error) { return error; }
    }"#;
    let compiled =
        compile_typescript_waffle(source, "completion.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    linker.instance("wasi:cli/types@0.3.0")?;
    let drained = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    let captured = Arc::new(Mutex::new(Vec::new()));
    let host_drained = drained.clone();
    let host_finish = finish.clone();
    let host_captured = captured.clone();
    linker.instance("wasi:cli/stdout@0.3.0")?.func_wrap(
        "write-via-stream",
        move |mut store: StoreContextMut<'_, StoreLimits>, (input,): (StreamReader<u8>,)| {
            let (closed, receiver) = oneshot::channel();
            input.pipe(
                &mut store,
                Capture {
                    bytes: host_captured.clone(),
                    closed: Some(closed),
                },
            )?;
            let drained = host_drained.clone();
            let finish = host_finish.clone();
            let completion = FutureReader::new(&mut store, async move {
                receiver.await?;
                drained.notify_one();
                finish.notified().await;
                wasmtime::error::Ok(Err::<(), _>(ErrorCode::IllegalByteSequence))
            })?;
            Ok((completion,))
        },
    )?;
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65_536).build(),
    );
    store.limiter(|limits| limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..30 {
        let mut invocation = Box::pin(run.call_async(&mut store, ()));
        tokio::select! {
            result = &mut invocation => panic!("returned before capability completion: {result:?}"),
            () = drained.notified() => {}
        }
        assert!(captured.lock().unwrap().ends_with(&[0, 128, 255]));
        assert!(
            timeout(Duration::from_millis(1), &mut invocation)
                .await
                .is_err()
        );
        finish.notify_one();
        assert_eq!(timeout(Duration::from_secs(5), invocation).await??.0, 2.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

struct Capture {
    bytes: Arc<Mutex<Vec<u8>>>,
    closed: Option<oneshot::Sender<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some(sender) = self.closed.take() {
            let _ = sender.send(());
        }
    }
}

impl StreamConsumer<StoreLimits> for Capture {
    type Item = u8;
    fn poll_consume(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        store: StoreContextMut<'_, StoreLimits>,
        source: Source<u8>,
        _: bool,
    ) -> Poll<wasmtime::Result<StreamResult>> {
        let mut source = source.as_direct(store);
        let bytes = source.remaining();
        let length = bytes.len();
        self.bytes.lock().unwrap().extend_from_slice(bytes);
        source.mark_read(length);
        Poll::Ready(Ok(StreamResult::Completed))
    }
}
