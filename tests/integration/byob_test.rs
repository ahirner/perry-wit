//! BYOB regressions ported from main: transferred storage, queued reads and cancellation.
use crate::world::Compiler::MemoryChecked;
use anyhow::Result;
use std::sync::{Arc, atomic::Ordering};
use tokio::sync::mpsc;
use wasmtime::{
    Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, ComponentType, Lift, Linker, StreamProducer, StreamReader, TypedFunc},
};
#[path = "../support/p3_input.rs"]
mod input;

use crate::engine::async_engine;
use input::{ControlledProducer, Observations};

const WIT: &str = r#"
package test:byob;
world run {
    export sum-bytes: async func(input: stream<u8>) -> result<f64>;
    export short-eof: async func(input: stream<u8>) -> result<list<u8>>;
    export pending-release: async func(input: stream<u8>) -> result<list<u8>>;
    export cancel-pending: async func(input: stream<u8>) -> result<f64>;
    export validate: async func(input: stream<u8>) -> result<f64>;
    export wait-min: async func(input: stream<u8>) -> result<list<u8>>;
    export buffered-queue: async func(input: stream<u8>) -> result<f64>;
    export recheck-aliases: async func(input: stream<u8>) -> result<f64>;
}
"#;

const SOURCE: &str = r#"
export async function sumBytes(input: ReadableStream<Uint8Array>): Promise<number> {
    const reader: ReadableStreamBYOBReader = input.getReader({mode: "byob"});
    let buffer = new Uint8Array(8196); buffer[0] = 19; buffer[8195] = 23; let total = 0;
    let done = false;
    while (!done) {
        const original = buffer.buffer; const alias = buffer.subarray(1, 2);
        const view = buffer.subarray(2, 8194);
        const pending = reader.read(view);
        if (buffer.length !== 0 || alias.length !== 0 || view.length !== 0 || original.byteLength !== 0) throw 91;
        let detached = false; try { new Uint8Array(original); } catch { detached = true; }
        if (!detached) throw 92;
        const result = await pending; const bytes = result.value;
        if (bytes === undefined) throw 93;
        if (bytes.byteOffset !== 2 || bytes.buffer.byteLength !== 8196) throw 94;
        for (let index = 0; index < bytes.length; index++) total += bytes[index];
        buffer = new Uint8Array(bytes.buffer);
        if (buffer[0] !== 19 || buffer[8195] !== 23) throw 95;
        done = result.done;
    }
    reader.releaseLock(); return total;
}

function openByob(input: ReadableStream<Uint8Array>) {
    return input.getReader({mode: "byob"});
}

type ReaderAlias = ReadableStreamDefaultReader;

export async function shortEof(input: ReadableStream<Uint8Array>): Promise<Uint8Array> {
    const opened = openByob(input) as ReaderAlias;
    const reader = opened as ReadableStreamBYOBReader;
    const result = await reader.read(new Uint8Array(8), {min: 8});
    const bytes = result.value; if (bytes === undefined) throw 1;
    const output = new Uint8Array(10); output[0] = result.done ? 1 : 0; output[1] = bytes.length;
    output.set(bytes, 2); await reader.cancel(); reader.releaseLock(); return output;
}

export async function pendingRelease(input: ReadableStream<Uint8Array>): Promise<Uint8Array> {
    const reader = input.getReader({mode: "byob"});
    const view = new Uint8Array(8); const pending = reader.read(view, {min: 8});
    const queued = reader.read(new Uint8Array(8)); reader.releaseLock();
    let rejected = 0; try { await pending; } catch { rejected++; }
    try { await queued; } catch { rejected++; }
    if (rejected !== 2 || view.length !== 0 || input.locked) throw 91;
    const next = input.getReader({mode: "byob"});
    const first = await next.read(new Uint8Array(2), {min: 2});
    const last = await next.read(new Uint8Array(6), {min: 6});
    const a = first.value; const b = last.value;
    if (a === undefined || b === undefined || first.done || !last.done) throw 92;
    const output = new Uint8Array(a.length + b.length); output.set(a); output.set(b, a.length);
    next.releaseLock(); return output;
}

export async function cancelPending(input: ReadableStream<Uint8Array>): Promise<number> {
    const reader = input.getReader({mode: "byob"}); const bytes = new Uint8Array(8);
    const pending = reader.read(bytes); const queued = reader.read(new Uint8Array(8)); await reader.cancel();
    const result = await pending;
    if (!result.done || result.value !== undefined || bytes.length !== 0) throw 91;
    const other = await queued; if (!other.done || other.value !== undefined) throw 93;
    const end = await reader.read(new Uint8Array(8)); const empty = end.value;
    if (!end.done || empty === undefined || empty.length !== 0) throw 92;
    reader.releaseLock(); return 1;
}

export async function validate(input: ReadableStream<Uint8Array>): Promise<number> {
    const reader = input.getReader({mode: "byob"}); const bytes = new Uint8Array(8);
    let failures = 0;
    try { await reader.read(new Uint8Array(0)); } catch { failures++; }
    try { await reader.read(bytes, {min: 0}); } catch { failures++; }
    try { await reader.read(bytes, {min: -1}); } catch { failures++; }
    try { await reader.read(bytes, {min: 9}); } catch { failures++; }
    try { await reader.read(bytes, {min: Infinity}); } catch { failures++; }
    try { await reader.read(bytes, {min: NaN}); } catch { failures++; }
    if (bytes.length !== 8) throw 91;
    reader.releaseLock();
    try { await reader.read(bytes); } catch { failures++; }
    if (bytes.length !== 8) throw 92;
    return failures;
}

export async function waitMin(input: ReadableStream<Uint8Array>): Promise<Uint8Array> {
    const reader = input.getReader({mode: "byob"});
    const result = await reader.read(new Uint8Array(8), {min: 4.9});
    const bytes = result.value; if (result.done || bytes === undefined) throw 91;
    await reader.cancel(); reader.releaseLock(); return bytes;
}

export async function bufferedQueue(input: ReadableStream<Uint8Array>): Promise<number> {
    const body = new Response(new Uint8Array([3, 7, 11, 19, 23])).body;
    if (body === null) throw 90;
    const reader = body.getReader({mode: "byob"});
    const a = await reader.read(new Uint8Array(2));
    const b = await reader.read(new Uint8Array(8), {min: 3});
    const first = a.value; const last = b.value;
    if (first === undefined || last === undefined || a.done || b.done) throw 91;
    if (first.length !== 2 || last.length !== 3 || first[1] !== 7 || last[2] !== 23) throw 92;
    const end = await reader.read(new Uint8Array(8));
    const empty = end.value; if (!end.done || empty === undefined || empty.length !== 0) throw 93;
    reader.releaseLock();
    const shortBody = new Response(new Uint8Array([1, 2, 3, 4, 5])).body;
    if (shortBody === null) throw 94;
    const shortReader = shortBody.getReader({mode: "byob"});
    await shortReader.read(new Uint8Array(2));
    let rejected = false; try { await shortReader.read(new Uint8Array(8), {min: 8}); } catch { rejected = true; }
    if (!rejected) throw 95;
    shortReader.releaseLock(); return 1;
}

async function consume(reader: ReadableStreamBYOBReader, bytes: Uint8Array<ArrayBuffer>): Promise<void> {
    await reader.read(bytes);
}

export async function recheckAliases(input: ReadableStream<Uint8Array>): Promise<number> {
    const reader = input.getReader({mode: "byob"});
    const first = new Uint8Array(8);
    const alias = first.subarray(0);
    for (let i = 0; i < 8; i++) {
        const pending = consume(reader, alias);
        if (first[i] !== undefined) throw 1;
        await pending;
        if (first[i] !== undefined) throw 2;
        break;
    }
    const second = new Uint8Array(8);
    const other = second.subarray(0);
    for (let i = 0; i < 8; i++) {
        const pending = reader.read(other);
        if (second[i] !== undefined) throw 3;
        await pending;
        if (second[i] !== undefined) throw 4;
        break;
    }
    const third = new Uint8Array(8);
    const view = third.subarray(0);
    for (let i = 0; i < third.length; i++) {
        const pending = consume(reader, view);
        if (third[i] !== undefined) throw 5;
        await pending;
        if (third[i] !== undefined) throw 6;
        break;
    }
    const fourth = new Uint8Array(8);
    const last = fourth.subarray(0);
    for (let i = 0; i < fourth.length; i++) {
        const pending = reader.read(last);
        if (fourth[i] !== undefined) throw 8;
        await pending;
        if (fourth[i] !== undefined) throw 9;
        break;
    }
    await reader.cancel(); reader.releaseLock(); return 7;
}
"#;

struct Byob<R> {
    store: Store<StoreLimits>,
    run: TypedFunc<(StreamReader<u8>,), (Result<R, ()>,)>,
}

impl<R: ComponentType + Lift + 'static> Byob<R> {
    async fn instantiate(
        engine: &Engine,
        component: &Component,
        linker: &Linker<StoreLimits>,
        export: &str,
        memory_size: usize,
    ) -> Result<Self> {
        let mut store = Store::new(
            engine,
            StoreLimitsBuilder::new().memory_size(memory_size).build(),
        );
        store.limiter(|limits| limits);
        let instance = linker.instantiate_async(&mut store, component).await?;
        let run = instance.get_typed_func(&mut store, export)?;
        Ok(Self { store, run })
    }

    async fn read(&mut self, producer: impl StreamProducer<StoreLimits, Item = u8>) -> Result<R> {
        let input = StreamReader::new(&mut self.store, producer)?;
        let (result,) = self.run.call_async(&mut self.store, (input,)).await?;
        self.store.assert_concurrent_state_empty();
        result.map_err(|()| anyhow::anyhow!("BYOB fixture returned an error"))
    }
}

fn check_typescript(source: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("byob.ts");
    std::fs::write(&path, source)?;
    let checked = std::process::Command::new("tsc")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/perry-frontend/src/sdk/runtime.d.ts"
        ))
        .current_dir(directory.path())
        .args(["--noEmit", "--strict", "--target", "ES2022"])
        .arg(path)
        .output()?;
    anyhow::ensure!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_transferred_storage_queued_reads_and_cancellation() -> Result<()> {
    check_typescript(SOURCE)?;
    let bytes = MemoryChecked.component(WIT, "run", "byob.ts", SOURCE)?;
    let engine = async_engine()?;
    let component = Component::new(&engine, bytes)?;
    let linker = Linker::new(&engine);

    // 1. Reused transferred buffers with offsets and bounded memory
    let mut sum_guest =
        Byob::<f64>::instantiate(&engine, &component, &linker, "sum-bytes", 65536).await?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024, 37] {
        let bytes: Vec<u8> = (0..size).map(|i| (i * 37 + i / 251) as u8).collect();
        let expected = bytes.iter().map(|&b| f64::from(b)).sum::<f64>();
        assert_eq!(sum_guest.read(bytes).await?, expected);
    }

    // 2. Minimum and short EOF preserve transferred bytes
    let mut short_guest =
        Byob::<Vec<u8>>::instantiate(&engine, &component, &linker, "short-eof", 65536).await?;
    for size in [0, 1, 7, 8, 9] {
        let bytes: Vec<u8> = (1..=size).collect();
        let read = usize::from(size.min(8));
        let mut expected = vec![u8::from(size < 8), size.min(8)];
        expected.extend_from_slice(&bytes[..read]);
        expected.resize(10, 0);
        assert_eq!(short_guest.read(bytes).await?, expected);
    }

    // 3. Pending release requeues bytes for a new reader
    let mut release_guest =
        Byob::<Vec<u8>>::instantiate(&engine, &component, &linker, "pending-release", 65536)
            .await?;
    for prefix in [0, 2] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        if prefix == 2 {
            sender.send(Ok(vec![3, 7])).await?;
        }
        let mut invocation = Box::pin(release_guest.read(ControlledProducer {
            receiver,
            observations: observations.clone(),
        }));
        tokio::select! {
            result = &mut invocation => panic!("completed before input: {result:?}"),
            () = observations.pending.notified() => {}
        }
        let feed = async move {
            sender
                .send(Ok(if prefix == 0 {
                    vec![3, 7, 11, 19, 23]
                } else {
                    vec![11, 19, 23]
                }))
                .await
                .unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(invocation, feed)
        })
        .await?;
        assert_eq!(result?, [3, 7, 11, 19, 23]);
        assert!(observations.dropped.load(Ordering::SeqCst));
    }

    // 4. Cancel settles pending reads without a view
    let mut cancel_guest =
        Byob::<f64>::instantiate(&engine, &component, &linker, "cancel-pending", 65536).await?;
    for _ in 0..3 {
        let observations = Arc::new(Observations::default());
        let (_sender, receiver) = mpsc::channel(1);
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                cancel_guest.read(ControlledProducer {
                    receiver,
                    observations: observations.clone(),
                })
            )
            .await??,
            1.0
        );
        assert!(observations.dropped.load(Ordering::SeqCst));
    }

    // 5. Validation rejects without transferring the buffer
    let mut validate_guest =
        Byob::<f64>::instantiate(&engine, &component, &linker, "validate", 65536).await?;
    for _ in 0..3 {
        assert_eq!(validate_guest.read(vec![1, 2, 3]).await?, 7.0);
    }

    // 6. Minimum waits across host releases
    let mut wait_min_guest =
        Byob::<Vec<u8>>::instantiate(&engine, &component, &linker, "wait-min", 65536).await?;
    for _ in 0..3 {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(vec![1, 2])).await?;
        let mut invocation = Box::pin(wait_min_guest.read(ControlledProducer {
            receiver,
            observations: observations.clone(),
        }));
        tokio::select! {
            result = &mut invocation => panic!("completed below minimum: {result:?}"),
            () = observations.pending.notified() => {}
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut invocation)
                .await
                .is_err()
        );
        sender.send(Ok(vec![3, 4])).await?;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), invocation).await??,
            [1, 2, 3, 4]
        );
    }

    // 7. Buffered BYOB reads keep offsets and reject short closed queues
    let mut buffered_guest =
        Byob::<f64>::instantiate(&engine, &component, &linker, "buffered-queue", 65536).await?;
    for _ in 0..10 {
        assert_eq!(buffered_guest.read(Vec::<u8>::new()).await?, 1.0);
    }

    // 8. Overlapping BYOB tasks keep independent buffers under collection
    let Byob { mut store, run } =
        Byob::<f64>::instantiate(&engine, &component, &linker, "sum-bytes", 512 * 1024).await?;
    for _ in 0..10 {
        let left = StreamReader::new(&mut store, vec![3u8; 65_539])?;
        let right = StreamReader::new(&mut store, vec![7u8; 131_079])?;
        let (left, right) = store
            .run_concurrent(async |accessor| {
                let (left, right) = tokio::join!(
                    run.call_concurrent(accessor, (left,)),
                    run.call_concurrent(accessor, (right,))
                );
                Ok::<_, wasmtime::Error>((left?.0.unwrap(), right?.0.unwrap()))
            })
            .await??;
        assert_eq!((left, right), (65_539.0 * 3.0, 131_079.0 * 7.0));
        store.assert_concurrent_state_empty();
    }

    // 9. Indexed reads recheck aliases after transfer and suspension
    let mut alias_guest =
        Byob::<f64>::instantiate(&engine, &component, &linker, "recheck-aliases", 65536).await?;
    for _ in 0..3 {
        assert_eq!(alias_guest.read(vec![9u8; 32]).await?, 7.0);
    }

    Ok(())
}
