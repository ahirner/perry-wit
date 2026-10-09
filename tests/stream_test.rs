//! `stream<u8>` export arguments as `ReadableStream<Uint8Array>`: reads that keep their chunks
//! get fresh objects, reads whose chunks die before the next read reuse one buffer (leases,
//! `perry_codegen::lease`), and both see the same bytes.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Linker, ResourceTable, StreamReader},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

const WIT: &str = r#"
package test:streams;
world streams {
  include wasi:cli/imports@0.3.0;
  export kept: async func(input: stream<u8>) -> f64;
  export leased: async func(input: stream<u8>) -> f64;
}
"#;

const SOURCE: &str = r#"
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

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn kept_and_leased_chunks_read_the_same_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("streams"))?;
    let compiled = compile_for_world(SOURCE, "streams.ts", &resolve, world, true)?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = wasmtime::component::Component::new(&engine, compiled.stripped.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            // One page: the leased reader must not keep a chunk per read.
            limits: StoreLimitsBuilder::new().memory_size(1 << 16).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let kept = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "kept")?;
    let leased = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "leased")?;

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
    let input = StreamReader::new(&mut store, small.clone())?;
    assert_eq!(
        kept.call_async(&mut store, (input,)).await?.0,
        weighted(&small)
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
    Ok(())
}
