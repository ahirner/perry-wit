use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wasmtime::component::StreamReader;

use super::{ControlledProducer, Observations, check_typescript, instantiate};

#[tokio::test(flavor = "current_thread")]
async fn reads_fill_only_the_visible_destination_prefix() -> Result<()> {
    let source = r#"
    declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
    async function fill(input: ByteStream, view: Uint8Array): Promise<number> {
        return await readInto(input, view);
    }
    export async function run(input: ByteStream, bytes: Uint8Array): Promise<Result<Uint8Array, number>> {
        const empty = bytes.subarray(bytes.length);
        if (await fill(input, empty) !== 0) { throw 1; }
        const view = bytes.subarray(2, 6);
        const count = await fill(input, view);
        bytes[0] = count;
        return bytes;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance
        .get_typed_func::<(StreamReader<u8>, Vec<u8>), (std::result::Result<Vec<u8>, f64>,)>(
            &mut store, "run",
        )?;
    for _ in 0..10 {
        for size in [0, 1, 3, 4, 5, 8193] {
            let bytes: Vec<u8> = (0..size).map(|index| (index * 83) as u8).collect();
            let count = size.min(4);
            let mut expected = vec![91u8; 8];
            expected[0] = count as u8;
            expected[2..2 + count].copy_from_slice(&bytes[..count]);
            let input = StreamReader::new(&mut store, bytes)?;
            assert_eq!(
                run.call_async(&mut store, (input, vec![91u8; 8])).await?.0,
                Ok(expected)
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn partial_reads_retain_subviews_across_suspension_and_collection() -> Result<()> {
    let source = r#"
    declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
    function destination(): Uint8Array {
        return new Uint8Array([9, 91, 91, 91, 91, 91, 7]).subarray(1, 6);
    }
    export async function run(input: ByteStream): Promise<Uint8Array> {
        const saved = destination();
        if (await readInto(input, saved.subarray(0, 0)) !== 0) { throw 1; }
        let total = 0;
        let count = await readInto(input, saved);
        while (count > 0) {
            total = total + count;
            let i = 0;
            while (i < 2000) {
                const scratch = new Uint8Array(33);
                scratch[0] = i;
                i = i + 1;
            }
            count = await readInto(input, saved.subarray(total));
        }
        return saved;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..3 {
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
        assert_eq!(observations.max_request.load(Ordering::SeqCst), 5);
        let feed = async move {
            for bytes in [vec![], vec![0, 255], vec![], vec![17], vec![]] {
                sender.send(Ok(bytes)).await.unwrap();
                tokio::task::yield_now().await;
            }
        };
        let (result, ()) = timeout(Duration::from_secs(5), async {
            tokio::join!(invocation, feed)
        })
        .await?;
        assert_eq!(result?.0, [0, 255, 17, 91, 91]);
        assert!(observations.dropped.load(Ordering::SeqCst));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn shared_transfers_reuse_bounded_storage_and_mix_with_chunk_reads() -> Result<()> {
    let source = r#"
    declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
    declare function readChunk(input: ByteStream): Promise<number>;
    declare function byteAt(index: number): number;
    export async function run(input: ByteStream): Promise<number> {
        const bytes = new Uint8Array(263);
        bytes[0] = 23;
        bytes[262] = 99;
        const view = bytes.subarray(3, 260);
        let total = 0;
        let count = await readChunk(input);
        let index = 0;
        while (index < count) {
            total = total + byteAt(index);
            index = index + 1;
        }
        count = await readInto(input, view);
        while (count > 0) {
            const scratch = new Uint8Array(128);
            scratch[0] = count;
            let index = 0;
            while (index < count) {
                total = total + view[index];
                index = index + 1;
            }
            count = await readInto(input, view);
        }
        if (bytes[0] !== 23) { return -1; }
        if (bytes[262] !== 99) { return -1; }
        if (await readChunk(input) !== 0) { return -2; }
        if (await readInto(input, view) !== 0) { return -2; }
        return total;
    }"#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source).await?;
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
async fn read_into_invalidates_the_previous_chunk() -> Result<()> {
    for capacity in [0, 1] {
        let source = format!(
            r#"
        declare function readChunk(input: ByteStream): Promise<number>;
        declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
        declare function byteAt(index: number): number;
        export async function run(input: ByteStream): Promise<number> {{
            const length = await readChunk(input);
            const count = await readInto(input, new Uint8Array({capacity}));
            return byteAt(0);
        }}"#
        );
        let (mut store, instance) = instantiate(&source).await?;
        let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
        let input = StreamReader::new(&mut store, vec![7u8; 8193])?;
        assert!(run.call_async(&mut store, (input,)).await.is_err());
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn returned_views_and_errors_survive_finally_transfers_before_cleanup() -> Result<()> {
    let source = r#"
    declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;
    export async function run(fail: boolean, input: ByteStream): Promise<Result<Uint8Array, number>> {
        const bytes = new Uint8Array(2);
        try {
            const count = await readInto(input, bytes);
            if (fail) { throw 91; }
            return bytes.subarray(0, count);
        } finally {
            let index = 0;
            while (index < 2000) {
                const scratch = new Uint8Array(64);
                scratch[0] = index;
                index = index + 1;
            }
            const count = await readInto(input, bytes);
            if (fail) { throw count + bytes[0]; }
        }
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance
        .get_typed_func::<(bool, StreamReader<u8>), (std::result::Result<Vec<u8>, f64>,)>(
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
            if fail { Err(33.0) } else { Ok(vec![32, 8]) }
        );
        assert_eq!(observations.bytes.load(Ordering::SeqCst), 3);
        assert!(observations.dropped.load(Ordering::SeqCst));
        assert!(sender.is_closed());
        store.assert_concurrent_state_empty();
    }
    Ok(())
}
