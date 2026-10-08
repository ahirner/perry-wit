#[path = "support/output_capture.rs"]
mod output_capture;
#[path = "support/waffle.rs"]
mod waffle_fixture;
use crate::output_capture::MemoryOutput;
use anyhow::Result;
use perry_wit::waffle_backend::WaffleCompileOptions;
use waffle_fixture::compile_typescript_waffle;
use wasmtime::component::{Component, Instance, Linker, ResourceTable, StreamReader};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

#[path = "waffle_output/completion.rs"]
mod completion;
#[path = "waffle_output/control.rs"]
mod control;

struct Host {
    context: WasiCtx,
    table: ResourceTable,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.context,
            table: &mut self.table,
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_output_preserves_arbitrary_bytes_and_subview_ranges_beyond_guest_memory()
-> Result<()> {
    let source = r#"
    import { Writable as Output } from "node:stream";
    import * as output from "node:stream";
    export async function run(count: number): Promise<number> {
        const storage = new Uint8Array(259);
        storage[0] = 71;
        storage[258] = 92;
        const bytes = storage.subarray(1, 258);
        let index = 0;
        while (index < bytes.length) { bytes[index] = index; index = index + 1; }
        await Output.toWeb(process.stdout).getWriter().write(new Uint8Array(0));
        index = 0;
        while (index < count) {
            await Output.toWeb(process.stdout).getWriter().write(bytes);
            const scratch = new Uint8Array(1024);
            scratch[0] = index;
            index = index + 1;
        }
        await output.Writable.toWeb(process.stderr).getWriter().write(storage.subarray(0, 1));
        return storage[0] + storage[258];
    }"#;
    let stdout = MemoryOutput::new(16 * 1024 * 1024);
    let stderr = MemoryOutput::new(1024);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "run")?;
    let mut expected = Vec::new();
    for count in [0, 1, 33, 16321, 1] {
        assert_eq!(
            run.call_async(&mut store, (f64::from(count),)).await?.0,
            163.0
        );
        for _ in 0..count {
            expected.extend((0..257).map(|index| index as u8));
        }
        assert_eq!(stdout.contents().as_ref(), expected);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(stderr.contents().as_ref(), &[71; 5]);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn string_logging_preserves_utf8_nuls_order_and_stored_task_lifetimes() -> Result<()> {
    let source = r#"
    async function message(text: string): Promise<string> {
        console.log(text);
        let index = 0;
        while (index < 2000) { const temporary = "😀" + text; index = index + 1; }
        console.error(text + "中");
        return text;
    }
    export async function run(): Promise<string> {
        const pending = message("é😀\0");
        console.warn("between");
        const result = await pending;
        return result + await pending;
    }"#;
    let stdout = MemoryOutput::new(8192);
    let stderr = MemoryOutput::new(8192);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (String,)>(&mut store, "run")?;
    for _ in 0..30 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, "é😀\0é😀\0");
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(stdout.contents().as_ref(), "é😀\0\n".repeat(30).as_bytes());
    let errors = String::from_utf8(stderr.contents().to_vec())?;
    assert_eq!(errors.matches("é😀\0中\n").count(), 30);
    assert_eq!(errors.matches("between\n").count(), 30);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn input_views_forward_to_output_without_buffering_the_full_stream() -> Result<()> {
    let source = r#"
    import {Writable} from "node:stream";
    export async function run(input:ReadableStream<Uint8Array>):Promise<number> {
        const reader=input.getReader();let total=0;let result=await reader.read();
        while(!result.done) {
            const bytes=result.value;if(bytes===undefined)throw 1;
            await Writable.toWeb(process.stdout).getWriter().write(bytes);total+=bytes.length;result=await reader.read();
        }
        reader.releaseLock();return total;
    }"#;
    let stdout = MemoryOutput::new(8 * 1024 * 1024);
    let context = WasiCtxBuilder::new().stdout(stdout.clone()).build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(StreamReader<u8>,), (f64,)>(&mut store, "run")?;
    let mut expected = Vec::new();
    for size in [0, 1, 4092, 4093, 4094, 4 * 1024 * 1024, 1] {
        let bytes: Vec<_> = (0..size)
            .map(|index| (index * 37 + index / 251) as u8)
            .collect();
        expected.extend_from_slice(&bytes);
        let input = StreamReader::new(&mut store, bytes)?;
        assert_eq!(run.call_async(&mut store, (input,)).await?.0, size as f64);
        assert_eq!(stdout.contents().as_ref(), expected);
        store.assert_concurrent_state_empty();
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn logging_argument_effects_and_stream_routing_match_node() -> Result<()> {
    let source = r#"
    function value(text: string): string { console.error("argument"); return text + "😀\0"; }
    export function run(): number {
        console.log(value("é"));
        console["warn"]("after");
        console.log("");
        return 3;
    }"#;
    let stdout = MemoryOutput::new(8192);
    let stderr = MemoryOutput::new(8192);
    let context = WasiCtxBuilder::new()
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    assert_eq!(run.call_async(&mut store, ()).await?.0, 3.0);
    let temporary = tempfile::tempdir()?;
    let path = temporary.path().join("log.mts");
    fs::write(&path, format!("{source}\nrun();"))?;
    let node = Command::new("node")
        .args([
            "--disable-warning=ExperimentalWarning",
            "--experimental-strip-types",
        ])
        .arg(path)
        .output()?;
    assert!(
        node.status.success(),
        "{}",
        String::from_utf8_lossy(&node.stderr)
    );
    assert_eq!(stdout.contents().as_ref(), node.stdout);
    assert_eq!(stderr.contents().as_ref(), node.stderr);
    store.assert_concurrent_state_empty();
    Ok(())
}

#[test]
fn output_contracts_and_binding_identity_are_checked_before_io() -> Result<()> {
    for source in [
        "export function run(): number { console.log(); return 0; }",
        "export function run(): number { console.log('a', 'b'); return 0; }",
        "export function run(): number { console.log(42); return 0; }",
        "export function run(): number { const write = console.log; write('a'); return 0; }",
        "export function run(name: string): number { console[name]('a'); return 0; }",
        "import {Writable} from 'node:stream'; export async function run(): Promise<number> { await Writable.toWeb(process.stdout).getWriter().write(true); return 0; }",
    ] {
        assert!(
            compile_typescript_waffle(source, "invalid.ts", &WaffleCompileOptions::default())
                .is_err(),
            "accepted: {source}"
        );
    }
    for source in [
        "function console(value: number): number { return value + 1; } export function run(): number { return console(3); }",
        "function outputValue(value: number): number { return value + 1; } export function run(): number { return outputValue(3); }",
        "import {Writable} from 'node:stream'; export function run(): number { return 3; }",
    ] {
        let compiled =
            compile_typescript_waffle(source, "shadow.ts", &WaffleCompileOptions::default())?;
        assert!(!compiled.component_wat.unwrap().contains("wasi:cli/"));
    }
    Ok(())
}

async fn instantiate(source: &str, context: WasiCtx) -> Result<(Store<Host>, Instance)> {
    let compiled =
        compile_typescript_waffle(source, "output.ts", &WaffleCompileOptions::default())?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    config.wasm_component_model_more_async_builtins(true);
    config.wasm_component_model_async_stackful(true);
    config.wasm_component_model_threading(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, compiled.component.unwrap())?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            context,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(65_536).build(),
        },
    );
    store.limiter(|host| &mut host.limits);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    Ok((store, instance))
}
use std::{fs, process::Command};

#[tokio::test(flavor = "current_thread")]
async fn stored_output_operations_settle_once_and_preserve_bytes() -> Result<()> {
    let source = "import {Writable} from 'node:stream'; export async function run(): Promise<number> { const pending = Writable.toWeb(process.stdout).getWriter().write(new Uint8Array([65,0,255])); await pending; await pending; return 1; }";
    let output = MemoryOutput::new(1024);
    let context = WasiCtxBuilder::new().stdout(output.clone()).build();
    let (mut store, instance) = instantiate(source, context).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..20 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 1.);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(output.contents().as_ref(), [65, 0, 255].repeat(20));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn web_writer_queues_writes_and_close_before_releasing_the_lock() -> Result<()> {
    let source = r#"
    import {Writable} from 'node:stream';
    export async function run():Promise<number> {
      const stream=Writable.toWeb(process.stdout);
      const writer=stream.getWriter();
      if(!stream.locked)throw 1;
      const a=new Uint8Array(2);a[0]=255;a[1]=0;
      const b=new Uint8Array(1);b[0]=71;
      const first=writer.write(a);
      const second=writer.write(b);
      const closed=writer.close();
      await first;await second;await closed;await first;
      let rejected=0;
      try {await writer.write(b);}catch{rejected++;}
      writer.releaseLock();
      if(stream.locked)throw 2;
      try{await writer.write(a);}catch{rejected++;}
      return rejected;
    }"#;
    let output = MemoryOutput::new(1000);
    let (mut store, instance) =
        instantiate(source, WasiCtxBuilder::new().stdout(output.clone()).build()).await?;
    let run = instance.get_typed_func::<(), (f64,)>(&mut store, "run")?;
    for _ in 0..30 {
        assert_eq!(run.call_async(&mut store, ()).await?.0, 2.);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    assert_eq!(output.contents().as_ref(), [255, 0, 71].repeat(30));
    Ok(())
}
