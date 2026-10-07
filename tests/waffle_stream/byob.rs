use anyhow::Result;
use wasmtime::component::StreamReader;

use super::check_typescript;

async fn instantiate(
    source: &str,
) -> Result<(
    wasmtime::Store<wasmtime::StoreLimits>,
    wasmtime::component::Instance,
)> {
    let compiled = super::waffle_fixture::compile_typescript_for_fixture_world(
        source,
        "byob.ts",
        &Default::default(),
    )?;
    super::instantiate_compiled(compiled).await
}

#[tokio::test(flavor = "current_thread")]
async fn byob_reuses_transferred_buffers_with_offsets_and_bounded_memory() -> Result<()> {
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader:ReadableStreamBYOBReader=input.getReader({mode:"byob"});
        let buffer=new Uint8Array(8196);buffer[0]=19;buffer[8195]=23;let total=0;
        let done=false;
        while(!done) {
            const original=buffer.buffer;const alias=buffer.subarray(1,2);
            const view=buffer.subarray(2,8194);
            const pending=reader.read(view);
            if(buffer.length!==0 || alias.length!==0 || view.length!==0 || original.byteLength!==0)throw 91;
            let detached=false;try {new Uint8Array(original);}catch {detached=true;}
            if(!detached)throw 92;
            const result=await pending;const bytes=result.value;
            if(bytes===undefined)throw 93;
            if(bytes.byteOffset!==2 || bytes.buffer.byteLength!==8196)throw 94;
            for(let index=0;index<bytes.length;index++)total+=bytes[index];
            buffer=new Uint8Array(bytes.buffer);
            if(buffer[0]!==19 || buffer[8195]!==23)throw 95;
            done=result.done;
        }
        reader.releaseLock();return total;
    }
    "#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024, 37] {
        let bytes: Vec<u8> = (0..size).map(|i| (i * 37 + i / 251) as u8).collect();
        let expected = bytes.iter().map(|&b| f64::from(b)).sum::<f64>();
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_minimum_and_short_eof_preserve_transferred_bytes() -> Result<()> {
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<Uint8Array> {
        const reader=input.getReader({mode:"byob"});
        const result=await reader.read(new Uint8Array(8),{min:8});
        const bytes=result.value;if(bytes===undefined)throw 1;
        const output=new Uint8Array(10);output[0]=result.done ? 1 : 0;output[1]=bytes.length;
        output.set(bytes,2);await reader.cancel();reader.releaseLock();return output;
    }
    "#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (Vec<u8>,)>(&mut store, "run")?;
    for size in [0, 1, 7, 8, 9] {
        let bytes: Vec<u8> = (1..=size).collect();
        let input = StreamReader::new(&mut store, bytes.clone())?;
        let mut expected = vec![0; 10];
        expected[0] = u8::from(size < 8);
        expected[1] = size.min(8);
        expected[2..2 + usize::from(size.min(8))]
            .copy_from_slice(&bytes[..usize::from(size.min(8))]);
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_pending_release_requeues_bytes_for_a_new_reader() -> Result<()> {
    use super::{ControlledProducer, Observations};
    use std::sync::{Arc, atomic::Ordering};
    use tokio::sync::mpsc;
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<Uint8Array> {
        const reader=input.getReader({mode:"byob"});
        const view=new Uint8Array(8);const pending=reader.read(view,{min:8});
        const queued=reader.read(new Uint8Array(8));reader.releaseLock();
        let rejected=0;try {await pending;}catch {rejected++;}
        try {await queued;}catch {rejected++;}
        if(rejected!==2 || view.length!==0 || input.locked)throw 91;
        const next=input.getReader({mode:"byob"});
        const first=await next.read(new Uint8Array(2),{min:2});
        const last=await next.read(new Uint8Array(6),{min:6});
        const a=first.value;const b=last.value;
        if(a===undefined || b===undefined || first.done || !last.done)throw 92;
        const output=new Uint8Array(a.length+b.length);output.set(a);output.set(b,a.length);
        next.releaseLock();return output;
    }
    "#;
    check_typescript(source)?;
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
            result=&mut invocation=>panic!("completed before input: {result:?}"),
            ()=observations.pending.notified()=>{}
        }
        let feed = async move {
            sender.send(Ok(vec![3, 7, 11, 19, 23])).await.unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(invocation, feed)
        })
        .await?;
        assert_eq!(result?.0, [3, 7, 11, 19, 23]);
        assert!(observations.dropped.load(Ordering::SeqCst));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_cancel_settles_pending_reads_without_a_view() -> Result<()> {
    use super::{ControlledProducer, Observations};
    use std::sync::{Arc, atomic::Ordering};
    use tokio::sync::mpsc;
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader=input.getReader({mode:"byob"});const bytes=new Uint8Array(8);
        const pending=reader.read(bytes);const queued=reader.read(new Uint8Array(8));await reader.cancel();
        const result=await pending;
        if(!result.done || result.value!==undefined || bytes.length!==0)throw 91;
        const other=await queued;if(!other.done || other.value!==undefined)throw 93;
        const end=await reader.read(new Uint8Array(8));const empty=end.value;
        if(!end.done || empty===undefined || empty.length!==0)throw 92;
        reader.releaseLock();return 1;
    }
    "#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        let observations = Arc::new(Observations::default());
        let (_sender, receiver) = mpsc::channel(1);
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                run.call_async(&mut store, (input,))
            )
            .await??
            .0,
            1.0
        );
        assert!(observations.dropped.load(Ordering::SeqCst));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_validation_rejects_without_transferring_the_buffer() -> Result<()> {
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader=input.getReader({mode:"byob"});const bytes=new Uint8Array(8);
        let failures=0;
        try {await reader.read(new Uint8Array(0));}catch {failures++;}
        try {await reader.read(bytes,{min:0});}catch {failures++;}
        try {await reader.read(bytes,{min:-1});}catch {failures++;}
        try {await reader.read(bytes,{min:9});}catch {failures++;}
        try {await reader.read(bytes,{min:Infinity});}catch {failures++;}
        try {await reader.read(bytes,{min:NaN});}catch {failures++;}
        if(bytes.length!==8)throw 91;
        reader.releaseLock();
        try {await reader.read(bytes);}catch {failures++;}
        if(bytes.length!==8)throw 92;
        return failures;
    }
    "#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![1, 2, 3])?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, 7.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_minimum_waits_across_host_releases() -> Result<()> {
    use super::{ControlledProducer, Observations};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<Uint8Array> {
        const reader=input.getReader({mode:"byob"});
        const result=await reader.read(new Uint8Array(8),{min:4.9});
        const bytes=result.value;if(result.done || bytes===undefined)throw 91;
        await reader.cancel();reader.releaseLock();return bytes;
    }"#;
    let (mut store, instance) = instantiate(source).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (Vec<u8>,)>(&mut store, "run")?;
    for _ in 0..3 {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        sender.send(Ok(vec![1, 2])).await?;
        let input = StreamReader::new(
            &mut store,
            ControlledProducer {
                receiver,
                observations: observations.clone(),
            },
        )?;
        let mut invocation = Box::pin(run.call_async(&mut store, (input,)));
        tokio::select! {
            result=&mut invocation=>panic!("completed below minimum: {result:?}"),
            ()=observations.pending.notified()=>{}
        }
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut invocation)
                .await
                .is_err()
        );
        sender.send(Ok(vec![3, 4])).await?;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), invocation)
                .await??
                .0,
            [1, 2, 3, 4]
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}
