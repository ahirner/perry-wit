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
    export async function run(input: ReadableStream<Uint8Array>, bytes: Uint8Array): Promise<Result<Uint8Array, number>> {
        const reader=input.getReader();const result=await reader.read();
        const chunk=result.value;let count=0;
        if(chunk!==undefined) {
            const view=bytes.subarray(2,6);count=(view.length<chunk.length ? view.length : chunk.length);
            for(let index=0;index<count;index++) {view[index]=chunk[index];}
        }
        await reader.cancel();reader.releaseLock();bytes[0]=count;return bytes;
    }
    "#;
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
    function destination():Uint8Array {return new Uint8Array([9,91,91,91,91,91,7]).subarray(1,6);}
    export async function run(input:ReadableStream<Uint8Array>):Promise<Uint8Array> {
        const saved=destination();const reader=input.getReader();let total=0;
        let result=await reader.read();
        while(!result.done) {
            const bytes=result.value;if(bytes===undefined)throw 1;
            const view=saved.subarray(total);
            for(let index=0;index<bytes.length;index++) {view[index]=bytes[index];}
            total+=bytes.length;
            for(let i=0;i<2000;i++) {const scratch=new Uint8Array(33);scratch[0]=i;}
            result=await reader.read();
        }
        reader.releaseLock();return saved;
    }
    "#;
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
        assert_eq!(observations.max_request.load(Ordering::SeqCst), 8192);
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
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const bytes=new Uint8Array(263);bytes[0]=23;bytes[262]=99;
        const view=bytes.subarray(3,260);const reader=input.getReader();let total=0;
        let result=await reader.read();
        while(!result.done) {
            const chunk=result.value;if(chunk===undefined)throw 1;
            for(let offset=0;offset<chunk.length;offset+=view.length) {
                const count=(view.length<chunk.length-offset ? view.length : chunk.length-offset);
                for(let index=0;index<count;index++) {view[index]=chunk[offset+index];}
                const scratch=new Uint8Array(128);scratch[0]=count;
                for(let index=0;index<count;index++) {total+=view[index];}
            }
            result=await reader.read();
        }
        if(bytes[0]!==23 || bytes[262]!==99)return -1;
        const eof=await reader.read();if(!eof.done)return -2;
        reader.releaseLock();return total;
    }
    "#;
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
async fn retained_chunks_survive_subsequent_reads() -> Result<()> {
    let source = r#"export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader=input.getReader();const first=await reader.read();
        const bytes=first.value;if(bytes===undefined)throw 1;
        await reader.read();await reader.cancel();reader.releaseLock();
        return bytes[0];
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![7u8; 8193])?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, 7.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn returned_views_and_errors_survive_finally_transfers_before_cleanup() -> Result<()> {
    let source = r#"
    export async function run(fail:boolean,input:ReadableStream<Uint8Array>):Promise<Result<Uint8Array,number>> {
        const bytes=new Uint8Array(2);const reader=input.getReader();
        try {
            const first=await reader.read();const chunk=first.value;if(chunk===undefined)throw 1;
            for(let i=0;i<chunk.length;i++) {bytes[i]=chunk[i];}
            if(fail)throw 91;
            return bytes.subarray(0,chunk.length);
        } finally {
            for(let index=0;index<2000;index++) {const scratch=new Uint8Array(64);scratch[0]=index;}
            const second=await reader.read();const chunk=second.value;if(chunk===undefined)throw 1;
            for(let i=0;i<chunk.length;i++) {bytes[i]=chunk[i];}
            await reader.cancel();reader.releaseLock();
            if(fail)throw chunk.length+bytes[0];
        }
    }
    "#;
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
