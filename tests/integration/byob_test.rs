//! BYOB regressions ported from main: transferred storage, queued reads and cancellation.
use anyhow::Result;
use wasmtime::{
    Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, Instance, Linker, StreamReader},
};
#[path = "../support/p3_input.rs"]
mod input;

use crate::engine::async_engine;

async fn instantiate(source: &str, result: &str) -> Result<(Store<StoreLimits>, Instance)> {
    let wit = format!(
        "package test:byob; world run {{ export run: async func(input: stream<u8>) -> result<{result}>; }}"
    );
    let (resolve, package) = perry_wit::component::wit::resolve_source("byob.wit", &wit)?;
    let world = resolve.select_world(&[package], Some("run"))?;
    let compiled = perry_wit::compiler::compile_for_world_with_memory_checks(
        source, "byob.ts", &resolve, world,
    )?;
    let engine = async_engine()?;
    let component = Component::new(&engine, compiled.component.unwrap().stripped)?;
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
    let path = directory.path().join("byob.ts");
    std::fs::write(&path, source)?;
    let checked = std::process::Command::new("tsc")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/src/sdk/runtime.d.ts"))
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

const SUM_BYTES: &str = r#"
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

#[tokio::test(flavor = "current_thread")]
async fn byob_reuses_transferred_buffers_with_offsets_and_bounded_memory() -> Result<()> {
    let source = SUM_BYTES;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source, "f64").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
    for size in [0, 1, 8191, 8192, 8193, 4 * 1024 * 1024, 37] {
        let bytes: Vec<u8> = (0..size).map(|i| (i * 37 + i / 251) as u8).collect();
        let expected = bytes.iter().map(|&b| f64::from(b)).sum::<f64>();
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0.unwrap(),
            expected
        );
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
    let (mut store, instance) = instantiate(source, "list<u8>").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<Vec<u8>, ()>,)>(
        &mut store, "run",
    )?;
    for size in [0, 1, 7, 8, 9] {
        let bytes: Vec<u8> = (1..=size).collect();
        let read = usize::from(size.min(8));
        let mut expected = vec![u8::from(size < 8), size.min(8)];
        expected.extend_from_slice(&bytes[..read]);
        expected.resize(10, 0);
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0.unwrap(),
            expected
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_pending_release_requeues_bytes_for_a_new_reader() -> Result<()> {
    use input::{ControlledProducer, Observations};
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
    let (mut store, instance) = instantiate(source, "list<u8>").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<Vec<u8>, ()>,)>(
        &mut store, "run",
    )?;
    for prefix in [0, 2] {
        let observations = Arc::new(Observations::default());
        let (sender, receiver) = mpsc::channel(1);
        if prefix == 2 {
            sender.send(Ok(vec![3, 7])).await?;
        }
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
        assert_eq!(result?.0.unwrap(), [3, 7, 11, 19, 23]);
        assert!(observations.dropped.load(Ordering::SeqCst));
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_cancel_settles_pending_reads_without_a_view() -> Result<()> {
    use input::{ControlledProducer, Observations};
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
    let (mut store, instance) = instantiate(source, "f64").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
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
            .0
            .unwrap(),
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
    let (mut store, instance) = instantiate(source, "f64").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![1, 2, 3])?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0.unwrap(), 7.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn byob_minimum_waits_across_host_releases() -> Result<()> {
    use input::{ControlledProducer, Observations};
    use std::sync::Arc;
    use tokio::sync::mpsc;
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<Uint8Array> {
        const reader=input.getReader({mode:"byob"});
        const result=await reader.read(new Uint8Array(8),{min:4.9});
        const bytes=result.value;if(result.done || bytes===undefined)throw 91;
        await reader.cancel();reader.releaseLock();return bytes;
    }"#;
    let (mut store, instance) = instantiate(source, "list<u8>").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<Vec<u8>, ()>,)>(
        &mut store, "run",
    )?;
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
                .0
                .unwrap(),
            [1, 2, 3, 4]
        );
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn buffered_byob_reads_keep_offsets_and_reject_short_closed_queues() -> Result<()> {
    let source = r#"
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const body=new Response(new Uint8Array([3,7,11,19,23])).body;
        if(body===null)throw 90;
        const reader=body.getReader({mode:"byob"});
        const a=await reader.read(new Uint8Array(2));
        const b=await reader.read(new Uint8Array(8),{min:3});
        const first=a.value;const last=b.value;
        if(first===undefined||last===undefined||a.done||b.done)throw 91;
        if(first.length!==2||last.length!==3||first[1]!==7||last[2]!==23)throw 92;
        const end=await reader.read(new Uint8Array(8));
        const empty=end.value;if(!end.done||empty===undefined||empty.length!==0)throw 93;
        reader.releaseLock();
        const shortBody=new Response(new Uint8Array([1,2,3,4,5])).body;
        if(shortBody===null)throw 94;
        const shortReader=shortBody.getReader({mode:"byob"});
        await shortReader.read(new Uint8Array(2));
        let rejected=false;try{await shortReader.read(new Uint8Array(8),{min:8});}catch{rejected=true;}
        if(!rejected)throw 95;
        shortReader.releaseLock();return 1;
    }
    "#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source, "f64").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
    for _ in 0..10 {
        let input = StreamReader::new(&mut store, Vec::<u8>::new())?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0.unwrap(), 1.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn overlapping_byob_tasks_keep_independent_buffers_under_collection() -> Result<()> {
    let (mut store, instance) = instantiate(SUM_BYTES, "f64").await?;
    *store.data_mut() = StoreLimitsBuilder::new().memory_size(512 * 1024).build();
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
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
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_reads_recheck_aliases_after_transfer_and_suspension() -> Result<()> {
    let source = r#"
async function consume(reader: ReadableStreamBYOBReader, bytes: Uint8Array<ArrayBuffer>): Promise<void> {
  await reader.read(bytes);
}
export async function run(input: ReadableStream<Uint8Array>): Promise<number> {
  const reader = input.getReader({mode:"byob"});
  const first = new Uint8Array(8);
  const alias = first.subarray(0);
  for (let i = 0; i < first.length; i++) {
    const pending = consume(reader, alias);
    if (first[i] !== undefined) throw 1;
    await pending;
    if (first[i] !== undefined) throw 2;
    break;
  }
  const second = new Uint8Array(8);
  const other = second.subarray(0);
  for (let i = 0; i < second.length; i++) {
    const pending = reader.read(other);
    if (second[i] !== undefined) throw 3;
    await pending;
    if (second[i] !== undefined) throw 4;
    break;
  }
  await reader.cancel(); reader.releaseLock(); return 7;
}
"#;
    check_typescript(source)?;
    let (mut store, instance) = instantiate(source, "f64").await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, ()>,)>(
        &mut store, "run",
    )?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, vec![9u8; 32])?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0.unwrap(), 7.0);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}
