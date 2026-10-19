//! `stream<u8>` export arguments as `ReadableStream<Uint8Array>`: reads that keep their chunks
//! get fresh objects, reads whose chunks die before the next read reuse one buffer (leases,
//! `perry_codegen::lease`), and both see the same bytes.
use crate::world::Compiler::{MemoryChecked, Standard};
use anyhow::{Context, Result};
use wasmtime::{
    Store,
    component::{Linker, StreamReader},
};
use wasmtime_wasi::WasiCtx;

use crate::wasi_host::{self, Host};
use crate::{engine::async_engine, output_capture};

struct BlockedRead {
    acknowledged: std::sync::Arc<std::sync::atomic::AtomicBool>,
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for BlockedRead {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
impl wasmtime::component::StreamProducer<()> for BlockedRead {
    type Item = u8;
    type Buffer = Option<u8>;
    fn poll_produce<'a>(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: wasmtime::StoreContextMut<'a, ()>,
        _: wasmtime::component::Destination<'a, u8, Option<u8>>,
        finish: bool,
    ) -> std::task::Poll<wasmtime::Result<wasmtime::component::StreamResult>> {
        if finish {
            self.acknowledged
                .store(true, std::sync::atomic::Ordering::SeqCst);
            std::task::Poll::Ready(Ok(wasmtime::component::StreamResult::Cancelled))
        } else {
            std::task::Poll::Pending
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn pending_reads_acknowledge_cancellation_before_dropping_stream() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let bytes = Standard.component(
        "package test:detached; world run {export run:async func(input:stream<u8>, mode:u32)->result<u32>;}",
        "run",
        "detached.ts",
        r#"
export async function run(input:ReadableStream<Uint8Array>, mode:number):Promise<number> {
  if(mode>=4){
    const reader=input.getReader({mode:"byob"});
    const first=reader.read(new Uint8Array(4));
    const second=reader.read(new Uint8Array(4));
    try{
      if(mode===4) await Promise.all([first,second,Promise.reject(new Error('stop'))]);
      else await Promise.all([first,Promise.reject(new Error('stop'))]);
    }catch{}
    return 7;
  }
  const reader=input.getReader();const pending=reader.read();
  if(mode===1){await reader.cancel();const result=await pending;if(!result.done)throw 1;}
  if(mode===2){reader.releaseLock();try{await pending;throw 1;}catch(e){if(!(e instanceof TypeError))throw e;}}
  if(mode===3){try{await Promise.all([pending,Promise.reject(new Error('stop'))]);}catch{}}
  return 7;
}
"#,
    )?;
    let engine = async_engine()?;
    let component = wasmtime::component::Component::new(&engine, bytes)?;
    for mode in [0, 1, 2, 3, 4, 5] {
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine)
            .instantiate_async(&mut store, &component)
            .await?;
        let run = instance
            .get_typed_func::<(StreamReader<u8>, u32), (std::result::Result<u32, ()>,)>(
                &mut store, "run",
            )?;
        let acknowledged = Arc::new(AtomicBool::new(false));
        let dropped = Arc::new(AtomicBool::new(false));
        let input = StreamReader::new(
            &mut store,
            BlockedRead {
                acknowledged: acknowledged.clone(),
                dropped: dropped.clone(),
            },
        )?;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run.call_async(&mut store, (input, mode)),
        )
        .await?;
        if mode == 0 || mode == 5 {
            assert!(
                result?.0.is_err(),
                "unresolved detached read must fail the task"
            );
        } else {
            assert_eq!(result?.0.unwrap(), 7);
        }
        assert!(acknowledged.load(Ordering::SeqCst), "mode={mode}");
        assert!(dropped.load(Ordering::SeqCst), "mode={mode}");
    }
    Ok(())
}

const WIT: &str = r#"
package test:streams;
world streams {
  include wasi:cli/imports@0.3.0;
  export kept: async func(input: stream<u8>) -> f64;
  export leased: async func(input: stream<u8>) -> f64;
  export iterated: async func(input: stream<u8>, stop: bool) -> f64;
  export interrupted: async func(input: stream<u8>) -> result<f64, string>;
}
"#;

const SOURCE: &str = r#"
export async function iterated(input: ReadableStream<Uint8Array>, stop: boolean): Promise<number> {
  let total = 0;
  let index = 0;
  for await (const bytes of input) {
    index++;
    if (stop) break;
    for (let i = 0; i < bytes.length; i++) total += bytes[i] * index;
    continue;
  }
  if (input.locked) throw 9;
  return total;
}
export async function interrupted(input: ReadableStream<Uint8Array>): Promise<number> {
  try {
    for await (const bytes of input) { if (bytes.length > 0) throw "stopped"; }
  } catch (error) { if (input.locked) throw "locked"; throw error; }
  return 0;
}
// Every chunk stays reachable: each read needs its own buffer.
export async function kept(input: ReadableStream<Uint8Array>): Promise<number> {
  const reader = input.getReader();
  const chunks: Uint8Array[] = [];
  let result = await reader.read();
  while (!result.done) {
    const bytes = result.value;
    if (bytes === undefined) throw 1;
    chunks.push(bytes);
    result = await reader.read();
  }
  reader.releaseLock();
  let sum = 0;
  for (let c = 0; c < chunks.length; c++) {
    const chunk = chunks[c];
    if (chunk === undefined) throw 2;
    for (let i = 0; i < chunk.length; i++) { const b = chunk[i]; if (b !== undefined) sum += b * (c + 1); }
  }
  return sum;
}

// Each chunk is dead before the next read: one buffer serves the whole stream.
export async function leased(input: ReadableStream<Uint8Array>): Promise<number> {
  const reader = input.getReader();
  let sum = 0;
  let index = 1;
  let result = await reader.read();
  while (!result.done) {
    const bytes = result.value;
    if (bytes === undefined) throw 1;
    const tail = bytes.subarray(1);
    const head = bytes[0];
    if (head !== undefined) sum += head * index;
    for (let i = 0; i < tail.length; i++) { const b = tail[i]; if (b !== undefined) sum += b * index; }
    index++;
    result = await reader.read();
  }
  reader.releaseLock();
  return sum;
}
"#;

#[tokio::test(flavor = "current_thread")]
async fn kept_and_leased_chunks_read_the_same_bytes() -> Result<()> {
    let bytes = Standard.component(WIT, "streams", "streams.ts", SOURCE)?;
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &bytes,
        // One page: the leased reader must not keep a chunk per read.
        Host::default().with_memory_limit(1 << 16),
    )
    .await?;
    let kept = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "kept")?;
    let leased = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "leased")?;
    let iterated =
        instance.get_typed_func::<(StreamReader<u8>, bool), (f64,)>(&mut store, "iterated")?;
    let interrupted = instance
        .get_typed_func::<(StreamReader<u8>,), (std::result::Result<f64, String>,)>(
            &mut store,
            "interrupted",
        )?;

    // Chunks arrive 8192 bytes at a time; weights tell their order apart.
    let weighted = |bytes: &[u8]| -> f64 {
        bytes
            .chunks(8192)
            .enumerate()
            .map(|(i, c)| {
                c.iter()
                    .map(|&b| f64::from(b) * (i + 1) as f64)
                    .sum::<f64>()
            })
            .sum()
    };
    let small: Vec<u8> = (0..20_000u32).map(|i| (i * 7 + i / 300) as u8).collect();
    let expected = weighted(&small);
    let input = StreamReader::new(&mut store, small)?;
    assert_eq!(
        kept.call_async(&mut store, (input,))
            .await
            .map_err(anyhow::Error::from)
            .context("retained chunks")?
            .0,
        expected
    );
    // 2 MiB through 64 KiB of memory.
    let large: Vec<u8> = (0..(2u32 << 20))
        .map(|i| (i * 13 + i / 8192) as u8)
        .collect();
    for _ in 0..2 {
        let input = StreamReader::new(&mut store, large.clone())?;
        assert_eq!(
            leased.call_async(&mut store, (input,)).await?.0,
            weighted(&large)
        );
        assert!(store.data().table.is_empty(), "host resources leaked");
    }
    let input = StreamReader::new(&mut store, large.clone())?;
    assert_eq!(
        iterated.call_async(&mut store, (input, false)).await?.0,
        weighted(&large)
    );
    let input = StreamReader::new(&mut store, large.clone())?;
    assert_eq!(iterated.call_async(&mut store, (input, true)).await?.0, 0.0);
    let input = StreamReader::new(&mut store, large)?;
    assert_eq!(
        interrupted.call_async(&mut store, (input,)).await?.0,
        Err("stopped".into())
    );
    store.assert_concurrent_state_empty();
    assert!(store.data().table.is_empty(), "host resources leaked");
    Ok(())
}

/// The plain build of this program is the `web.streams.byob_reader` conformance contract; here
/// forced collection and poisoning run while transferred buffers and writes are in flight.
#[tokio::test(flavor = "current_thread")]
async fn byob_reads_and_acknowledged_writes_reuse_one_page() -> Result<()> {
    let source = r#"
    import {Writable} from 'node:stream';
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader=input.getReader({mode:"byob"});
        const writer=Writable.toWeb(process.stdout).getWriter();
        let buffer=new Uint8Array(8196);buffer[0]=19;buffer[8195]=23;let count=0;
        while(true) {
            const result=await reader.read(buffer.subarray(2,8194),{min:8192});
            const bytes=result.value;if(bytes===undefined)throw 1;
            buffer=new Uint8Array(bytes.buffer);
            if(buffer[0]!==19 || buffer[8195]!==23)throw 2;
            if(bytes.length>0) {
                const pending=writer.write(bytes);await pending;await pending;count+=bytes.length;
            }
            if(result.done)break;
        }
        writer.releaseLock();reader.releaseLock();return count;
    }
    "#;
    let component = MemoryChecked.component(
        "package test:echo; world echo { include wasi:cli/imports@0.3.0; export run:async func(input:stream<u8>)->f64; }",
        "echo",
        "echo.ts",
        source,
    )?;
    let bytes: Vec<u8> = (0..4 * 1024 * 1024)
        .map(|i| (i * 37 + i / 251) as u8)
        .collect();
    let output = output_capture::MemoryOutput::new(bytes.len() * 3);
    let (mut store, instance) = wasi_host::instantiate(
        &async_engine()?,
        &component,
        Host::new(WasiCtx::builder().stdout(output.clone()).build()).with_memory_limit(65536),
    )
    .await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    for _ in 0..3 {
        let input = StreamReader::new(&mut store, bytes.clone())?;
        assert_eq!(
            run.call_async(&mut store, (input,)).await?.0,
            bytes.len() as f64
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(output.contents().as_ref(), bytes.repeat(3));
    Ok(())
}
