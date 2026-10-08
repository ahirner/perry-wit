use std::sync::{Arc, atomic::Ordering};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wasmtime::component::StreamReader;

use super::{ControlledProducer, Observations, instantiate};

const SCAN_SCALARS: &str = r#"
export async function run(input: ReadableStream<Uint8Array>): Promise<Result<number, number>> {
    const decoder=new TextDecoder('utf-8',{fatal:true});
    const reader=input.getReader();let total=0;
    let chunk=await reader.read();
    while(!chunk.done) {
        const bytes=chunk.value;if(bytes===undefined)throw 1;
        const text=decoder.decode(bytes,{stream:true});
        for(let index=0;index<8;index++) {const scratch=new Uint8Array(128);scratch[0]=index;}
        total+=text.length;
        chunk=await reader.read();
    }
    total+=decoder.decode().length;
    reader.releaseLock();return total;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn text_larger_than_guest_memory_decodes_incrementally_and_reclaims_storage() -> Result<()> {
    let (mut store, instance) = instantiate(SCAN_SCALARS).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, f64>,)>(
        &mut store, "run",
    )?;
    for _ in 0..3 {
        for repetitions in [0, 1, 255, 4096, 131072] {
            let text = "\u{feff}😀é漢A".repeat(repetitions);
            let expected = text.chars().count() - usize::from(!text.is_empty());
            let input = StreamReader::new(&mut store, text.into_bytes())?;
            assert_eq!(
                run.call_async(&mut store, (input,)).await?.0,
                Ok(expected as f64)
            );
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn partial_scalars_survive_pending_reads_and_input_buffer_reuse() -> Result<()> {
    let (mut store, instance) = instantiate(SCAN_SCALARS).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, f64>,)>(
        &mut store, "run",
    )?;
    let observations = Arc::new(Observations::default());
    let (sender, receiver) = mpsc::channel(1);
    sender.send(Ok(vec![0xe2])).await?;
    let input = StreamReader::new(
        &mut store,
        ControlledProducer {
            receiver,
            observations: observations.clone(),
        },
    )?;
    let mut invocation = Box::pin(run.call_async(&mut store, (input,)));
    tokio::select! {
        result = &mut invocation => panic!("completed during a split scalar: {result:?}"),
        () = observations.pending.notified() => {}
    }
    assert!(
        timeout(Duration::from_millis(10), &mut invocation)
            .await
            .is_err()
    );
    assert_eq!(observations.bytes.load(Ordering::SeqCst), 1);
    let feed = async move {
        for bytes in [vec![], vec![0x82, 0xac, 0xf0, 0x9f], vec![0x98, 0x80]] {
            sender.send(Ok(bytes)).await.unwrap();
        }
    };
    let (result, ()) = timeout(Duration::from_secs(5), async {
        tokio::join!(invocation, feed)
    })
    .await?;
    assert_eq!(result?.0, Ok(2.0));
    assert!(observations.dropped.load(Ordering::SeqCst));
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_and_unfinished_stream_text_uses_the_guest_error_channel() -> Result<()> {
    let (mut store, instance) = instantiate(SCAN_SCALARS).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, f64>,)>(
        &mut store, "run",
    )?;
    for _ in 0..30 {
        for bytes in [
            vec![65, 0xff, 66],
            vec![0xf0, 0x9f, 0x98],
            vec![0xed, 0xa0, 0x80],
            vec![65],
        ] {
            let expected = if bytes == [65] { Ok(1.0) } else { Err(2.0) };
            let input = StreamReader::new(&mut store, bytes)?;
            assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
            store.assert_concurrent_state_empty();
        }
    }
    Ok(())
}
