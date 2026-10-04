use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;
use std::{fs, process::Command};

use anyhow::Result;
use perry_wit::{compile_typescript_waffle, waffle_backend::WaffleCompileOptions};
use tokio::sync::mpsc;
use tokio::time::timeout;
use wasmtime::component::{Component, Linker, StreamReader};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};

#[path = "support/p3_input.rs"]
mod input;
use input::{ControlledProducer, Observations};

#[path = "waffle_stream/decoding.rs"]
mod decoding;
#[path = "waffle_stream/transfers.rs"]
mod transfers;

const SUM_BYTES: &str = r#"
declare function readChunk(input: ByteStream): Promise<number>;
declare function byteAt(index: number): number;
async function chunk(input: ByteStream): Promise<number> { return await readChunk(input); }
export async function run(input: ByteStream): Promise<number> {
    if (input) {} else { return -1; }
    let total = 0;
    let length = await chunk(input);
    while (length > 0) {
        let index = 0;
        while (index < length) {
            total = total + byteAt(index);
            index = index + 1;
        }
        length = await chunk(input);
    }
    return total;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn source_streams_read_more_than_guest_memory_and_reuse_the_instance() -> Result<()> {
    check_typescript(SUM_BYTES)?;
    let (mut store, instance) = instantiate(SUM_BYTES).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024, 37] {
        let bytes: Vec<u8> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        let expected = bytes.iter().map(|&byte| f64::from(byte)).sum::<f64>();
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn pending_empty_and_partial_chunks_preserve_demand_and_cleanup() -> Result<()> {
    let (mut store, instance) = instantiate(SUM_BYTES).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    let observations = Arc::new(Observations::default());
    let (sender, receiver) = mpsc::channel(1);
    let input = StreamReader::new(
        &mut store,
        ControlledProducer {
            receiver,
            observations: observations.clone(),
        },
    )?;
    let mut invocation = Box::pin(run.call_async(&mut store, (input,)));
    tokio::select! {
        result = &mut invocation => panic!("completed before input: {result:?}"),
        () = observations.pending.notified() => {}
    }
    assert!(
        timeout(Duration::from_millis(10), &mut invocation)
            .await
            .is_err()
    );
    assert_eq!(observations.bytes.load(Ordering::SeqCst), 0);
    let feed = async move {
        for bytes in [vec![], vec![0, 255], vec![], vec![17], vec![]] {
            sender.send(Ok(bytes)).await.unwrap();
            tokio::task::yield_now().await;
        }
        drop(sender);
    };
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(invocation, feed)
    })
    .await?;
    assert_eq!(result?.0, 272.0);
    assert_eq!(observations.bytes.load(Ordering::SeqCst), 3);
    assert_eq!(observations.max_request.load(Ordering::SeqCst), 8192);
    assert!(observations.dropped.load(Ordering::SeqCst));
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn early_returns_and_numeric_errors_drop_once_after_finally() -> Result<()> {
    let source = r#"
    declare function readChunk(input: ByteStream): Promise<number>;
    declare function byteAt(index: number): number;
    export async function run(fail: boolean, input: ByteStream): Promise<Result<number, number>> {
        let value = 0;
        try {
            value = await readChunk(input);
            if (fail) { throw 91; }
            return value;
        } finally {
            value = value + await readChunk(input);
            if (fail) { throw value + byteAt(0); }
        }
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance
        .get_typed_func::<(bool, StreamReader<u8>), (std::result::Result<f64, f64>,)>(
            &mut store, "run",
        )?;
    for fail in [false, true, false, true] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(3);
        sender.send(Ok(vec![4, 8])).await?;
        sender.send(Ok(vec![32])).await?;
        sender.send(Ok(vec![99])).await?;
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        assert_eq!(
            run.call_async(&mut store, (fail, input)).await?.0,
            if fail { Err(35.0) } else { Ok(2.0) }
        );
        assert_eq!(observations.bytes.load(Ordering::SeqCst), 3);
        assert!(observations.dropped.load(Ordering::SeqCst));
        assert!(sender.is_closed());
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn producer_traps_and_store_disposal_release_pending_streams() -> Result<()> {
    for host_error in [false, true] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        let (mut store, instance) = instantiate(SUM_BYTES).await?;
        let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        if host_error {
            sender.send(Err("file read failed".into())).await?;
        }
        let mut invocation = Box::pin(run.call_async(&mut store, (input,)));
        if host_error {
            let error = invocation.await.unwrap_err();
            assert!(format!("{error:#}").contains("file read failed"));
        } else {
            tokio::select! {
                result = &mut invocation => panic!("completed before input: {result:?}"),
                () = observations.pending.notified() => {}
            }
            drop(invocation);
        }
        drop(store);
        assert!(observations.dropped.load(Ordering::SeqCst));
        assert!(sender.is_closed());
    }
    Ok(())
}

#[test]
fn unsupported_stream_ownership_is_diagnosed() {
    for source in [
        "declare function byteAt(index: number): number; export function run(): number { return byteAt(0); }",
        "export function run(a: ByteStream, b: ByteStream): number { return 0; }",
        "declare function readChunk(input: ByteStream): Promise<number>; export async function run(input: ByteStream): Promise<number> { const pending = readChunk(input); return await pending; }",
        "declare function readChunk(input: ByteStream): Promise<number>; export async function run(input: ByteStream): Promise<number> { return await readChunk(7); }",
        "declare function readChunk(input: ByteStream): Promise<number>; export async function run(input: ByteStream): Promise<number> { return await readChunk(true); }",
        "declare function readChunk(input: ByteStream): Promise<number>; async function read(input: ByteStream): Promise<number> { return await readChunk(input); } export async function run(input: ByteStream): Promise<number> { return await read(new Uint8Array(1)); }",
        "declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>; export async function run(input: ByteStream): Promise<number> { return await readInto(input, true); }",
        "declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>; export async function run(input: ByteStream): Promise<number> { return await readInto(new Uint8Array(1), new Uint8Array(1)); }",
        "declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>; export async function run(input: ByteStream): Promise<number> { const pending = readInto(input, new Uint8Array(1)); return await pending; }",
    ] {
        assert!(
            compile_typescript_waffle(
                source,
                "invalid-stream.ts",
                &WaffleCompileOptions::default()
            )
            .is_err(),
            "{source}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn overlapping_calls_do_not_share_the_active_buffer() -> Result<()> {
    let (mut store, instance) = instantiate(SUM_BYTES).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    let observations = [
        Arc::new(Observations::default()),
        Arc::new(Observations::default()),
    ];
    let (first_sender, first_receiver) = mpsc::channel(1);
    let (second_sender, second_receiver) = mpsc::channel(1);
    second_sender.send(Ok(vec![99])).await?;
    let first = StreamReader::new(
        &mut store,
        ControlledProducer {
            receiver: first_receiver,
            observations: observations[0].clone(),
        },
    )?;
    let second = StreamReader::new(
        &mut store,
        ControlledProducer {
            receiver: second_receiver,
            observations: observations[1].clone(),
        },
    )?;
    let result = timeout(
        Duration::from_secs(5),
        store.run_concurrent(async |accessor| {
            let mut pending = Box::pin(run.call_concurrent(accessor, (first,)));
            assert!(
                timeout(Duration::from_millis(10), &mut pending)
                    .await
                    .is_err()
            );
            let mut other = Box::pin(run.call_concurrent(accessor, (second,)));
            assert!(
                timeout(Duration::from_millis(10), &mut other)
                    .await
                    .is_err()
            );
            assert_eq!(observations[1].bytes.load(Ordering::SeqCst), 0);
            first_sender.send(Ok(vec![7])).await.unwrap();
            drop(first_sender);
            drop(second_sender);
            tokio::join!(pending, other)
        }),
    )
    .await?;
    let (first, second) = result?;
    assert_eq!(first?.0, 7.0);
    assert_eq!(second?.0, 99.0);
    drop(store);
    assert!(
        observations
            .iter()
            .all(|item| item.dropped.load(Ordering::SeqCst))
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byte_indices_cannot_read_outside_the_current_chunk() -> Result<()> {
    for index in ["-1", "0.5", "1", "8192", "NaN", "Infinity"] {
        let source = format!(
            r#"
        declare function readChunk(input: ByteStream): Promise<number>;
        declare function byteAt(index: number): number;
        export async function run(input: ByteStream): Promise<number> {{
            const length = await readChunk(input);
            return byteAt({index});
        }}"#
        );
        let (mut store, instance) = instantiate(&source).await?;
        let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
        let input = StreamReader::new(&mut store, vec![7u8])?;
        assert!(
            run.call_async(&mut store, (input,)).await.is_err(),
            "{index}"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn strings_survive_stream_reads_and_repeated_allocation() -> Result<()> {
    let source = r#"
    declare function readChunk(input: ByteStream): Promise<number>;
    export async function run(input: ByteStream): Promise<string> {
        let result = "😀é";
        let count = await readChunk(input);
        while (count > 0) {
            let index = 0;
            while (index < 2000) {
                result = ("x" + result).slice(1);
                index = index + 1;
            }
            count = await readChunk(input);
        }
        return result;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (String,)>(&mut store, "run")?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![7u8; 8193])?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, "😀é");
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn binary_views_remain_live_across_native_reads() -> Result<()> {
    let source = r#"
    declare function readChunk(input: ByteStream): Promise<number>;
    declare function byteAt(index: number): number;
    function retained(): Uint8Array { return new Uint8Array([0, 17, 0]).subarray(1, 2); }
    export async function run(input: ByteStream): Promise<number> {
        const saved = retained();
        let count = await readChunk(input);
        let total = 0;
        while (count > 0) {
            let index = 0;
            while (index < count) {
                const scratch = new Uint8Array([byteAt(index)]);
                total = total + scratch[0];
                if (saved[0] !== 17) { return -1; }
                index = index + 1;
            }
            count = await readChunk(input);
        }
        return total;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![255u8; 16385])?;
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0,
            255.0 * 16385.0
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

async fn instantiate(source: &str) -> Result<(Store<StoreLimits>, wasmtime::component::Instance)> {
    let compiled =
        compile_typescript_waffle(source, "stream.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    assert_eq!(component.component_type().imports(&engine).count(), 0);
    let mut store = Store::new(
        &engine,
        StoreLimitsBuilder::new().memory_size(65536).build(),
    );
    store.limiter(|limits| limits);
    let instance = Linker::new(&engine)
        .instantiate_async(&mut store, &component)
        .await?;
    Ok((store, instance))
}

fn check_typescript(source: &str) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("scan.ts");
    fs::write(&path, source)?;
    let checked = Command::new("tsc")
        .current_dir(directory.path())
        .args(["--noEmit", "--strict", "--target", "ES2022"])
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/types/p3.d.ts"))
        .arg(path)
        .output()?;
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stdout)
    );
    Ok(())
}
