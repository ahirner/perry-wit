use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncWrite;
use tokio::sync::Notify;
use tokio::time::timeout;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};

use super::instantiate;

const WRITE: &str = r#"
import {Writable} from "node:stream";
export async function run(): Promise<Result<number, number>> {
    const bytes = new Uint8Array([0,255,128,65,66,67,68]);
    try {
        await Writable.toWeb(process.stdout).getWriter().write(bytes.subarray(1, 6));
        return 7;
    } finally { await Writable.toWeb(process.stderr).getWriter().write(new Uint8Array([91])); }
}"#;

#[tokio::test(flavor = "current_thread")]
async fn suspended_writers_retain_text_and_completion_buffers_during_sibling_collection()
-> Result<()> {
    let source = r#"
    async function message(text: string): Promise<number> { console.log(text); return text.length; }
    export async function run(): Promise<number> {
        let text = "";
        let count = 0;
        while (count < 500) { text = text + "é😀"; count = count + 1; }
        const pending = message(text);
        let index = 0;
        while (index < 2000) { const temporary = new Uint8Array(1024); index = index + 1; }
        console.error("ready");
        return await pending;
    }"#;
    let stdout = ControlledOutput::new(WriteState::Blocked, FlushState::Ready);
    let stderr = ControlledOutput::new(WriteState::Accept(7), FlushState::Ready);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for iteration in 0..30 {
        stdout.set(WriteState::Blocked, FlushState::Ready);
        let mut invocation = Box::pin(run.call_async(&mut store, ()));
        tokio::select! {
            result = &mut invocation => panic!("output completed before readiness: {result:?}"),
            () = stdout.wait_for(|state| state.pending == Some(PendingOperation::Write)) => {}
        }
        assert!(
            timeout(Duration::from_millis(1), &mut invocation)
                .await
                .is_err()
        );
        tokio::select! {
            result = &mut invocation => panic!("output completed before sibling collection: {result:?}"),
            () = stderr.wait_for(|state| state.bytes.len() == (iteration + 1) * 6) => {}
        }
        stdout.set(WriteState::Accept(11), FlushState::Ready);
        assert_eq!(
            timeout(Duration::from_secs(5), invocation).await??.0,
            1000.0
        );
        store.assert_concurrent_state_empty();
    }
    assert_eq!(
        stdout.state.lock().unwrap().bytes,
        ("é😀".repeat(500) + "\n").repeat(30).into_bytes()
    );
    assert_eq!(stderr.state.lock().unwrap().bytes, b"ready\n".repeat(30));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn partial_writes_wait_for_readiness_and_flush_before_returning() -> Result<()> {
    let stdout = ControlledOutput::new(WriteState::Blocked, FlushState::Blocked);
    let stderr = ControlledOutput::new(WriteState::Accept(1), FlushState::Ready);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(WRITE, context).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    let mut invocation = Box::pin(run.call_async(&mut store, ()));
    tokio::select! {
        result = &mut invocation => panic!("output completed before readiness: {result:?}"),
        () = stdout.wait_for(|state| state.pending == Some(PendingOperation::Write)) => {}
    }
    assert!(stdout.state.lock().unwrap().bytes.is_empty());
    assert!(stderr.state.lock().unwrap().bytes.is_empty());
    stdout.set(WriteState::Accept(2), FlushState::Blocked);
    tokio::select! {
        result = &mut invocation => panic!("output completed before flush: {result:?}"),
        () = stdout.wait_for(|state| state.pending == Some(PendingOperation::Flush)) => {}
    }
    assert_eq!(stdout.state.lock().unwrap().bytes, [255, 128]);
    assert!(
        timeout(Duration::from_millis(10), &mut invocation)
            .await
            .is_err()
    );
    stdout.set(WriteState::Accept(2), FlushState::Ready);
    assert_eq!(
        timeout(Duration::from_secs(5), invocation).await??.0,
        Ok(7.0)
    );
    assert_eq!(stdout.state.lock().unwrap().bytes, [255, 128, 65, 66, 67]);
    assert_eq!(stderr.state.lock().unwrap().bytes, [91]);
    assert_eq!(stdout.state.lock().unwrap().dropped, 1);
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn transfer_and_flush_failures_reach_catch_and_finally_then_allow_reuse() -> Result<()> {
    let stdout = ControlledOutput::new(WriteState::Accept(2), FlushState::Ready);
    let stderr = ControlledOutput::new(WriteState::Accept(1), FlushState::Ready);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(WRITE, context).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    for _ in 0..30 {
        for (write, flush, expected) in [
            (
                WriteState::Fail(io::ErrorKind::BrokenPipe),
                FlushState::Ready,
                Err(3.0),
            ),
            (
                WriteState::Accept(2),
                FlushState::Fail(io::ErrorKind::Other),
                Err(1.0),
            ),
            (WriteState::Accept(2), FlushState::Ready, Ok(7.0)),
        ] {
            stdout.set(write, flush);
            assert_eq!(run.call_async(&mut store, ()).await?.0, expected);
            store.assert_concurrent_state_empty();
            assert!(store.data().table.is_empty());
        }
    }
    assert_eq!(stderr.state.lock().unwrap().bytes, vec![91; 90]);
    assert_eq!(stdout.state.lock().unwrap().dropped, 90);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn disposal_releases_blocked_output_without_running_stale_finally() -> Result<()> {
    let stdout = ControlledOutput::new(WriteState::Blocked, FlushState::Ready);
    let stderr = ControlledOutput::new(WriteState::Accept(1), FlushState::Ready);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(WRITE, context).await?;
    let run = instance.get_typed_func::<(), (Result<f64, f64>,)>(&mut store, "run")?;
    let mut invocation = Box::pin(run.call_async(&mut store, ()));
    tokio::select! {
        result = &mut invocation => panic!("output completed before readiness: {result:?}"),
        () = stdout.wait_for(|state| state.pending == Some(PendingOperation::Write)) => {}
    }
    drop(invocation);
    drop(store);
    assert_eq!(stdout.state.lock().unwrap().dropped, 1);
    assert!(stderr.state.lock().unwrap().bytes.is_empty());
    Ok(())
}

#[derive(Clone, Copy)]
enum WriteState {
    Blocked,
    Accept(usize),
    Fail(io::ErrorKind),
}
#[derive(Clone, Copy)]
enum FlushState {
    Blocked,
    Ready,
    Fail(io::ErrorKind),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingOperation {
    Write,
    Flush,
}

struct OutputState {
    write: WriteState,
    flush: FlushState,
    bytes: Vec<u8>,
    waker: Option<Waker>,
    pending: Option<PendingOperation>,
    dropped: usize,
}

#[derive(Clone)]
struct ControlledOutput {
    state: Arc<Mutex<OutputState>>,
    changed: Arc<Notify>,
}

impl ControlledOutput {
    fn new(write: WriteState, flush: FlushState) -> Self {
        Self {
            state: Arc::new(Mutex::new(OutputState {
                write,
                flush,
                bytes: Vec::new(),
                waker: None,
                pending: None,
                dropped: 0,
            })),
            changed: Arc::new(Notify::new()),
        }
    }

    async fn wait_for(&self, predicate: impl Fn(&OutputState) -> bool) {
        loop {
            let changed = self.changed.notified();
            if predicate(&self.state.lock().unwrap()) {
                return;
            }
            changed.await;
        }
    }

    fn set(&self, write: WriteState, flush: FlushState) {
        let waker = {
            let mut state = self.state.lock().unwrap();
            state.pending = None;
            state.write = write;
            state.flush = flush;
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

impl IsTerminal for ControlledOutput {
    fn is_terminal(&self) -> bool {
        false
    }
}
impl StdoutStream for ControlledOutput {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(Writer(self.clone()))
    }
}

struct Writer(ControlledOutput);
impl Drop for Writer {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().dropped += 1;
    }
}

impl AsyncWrite for Writer {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut state = self.0.state.lock().unwrap();
        match state.write {
            WriteState::Accept(max) => {
                let count = max.min(bytes.len());
                state.bytes.extend_from_slice(&bytes[..count]);
                state.pending = None;
                self.0.changed.notify_one();
                Poll::Ready(Ok(count))
            }
            WriteState::Fail(kind) => Poll::Ready(Err(kind.into())),
            WriteState::Blocked => {
                state.pending = Some(PendingOperation::Write);
                state.waker = Some(cx.waker().clone());
                self.0.changed.notify_one();
                Poll::Pending
            }
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut state = self.0.state.lock().unwrap();
        match state.flush {
            FlushState::Ready => Poll::Ready(Ok(())),
            FlushState::Fail(kind) => Poll::Ready(Err(kind.into())),
            FlushState::Blocked => {
                state.pending = Some(PendingOperation::Flush);
                state.waker = Some(cx.waker().clone());
                self.0.changed.notify_one();
                Poll::Pending
            }
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}
