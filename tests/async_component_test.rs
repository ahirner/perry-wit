//! Async component exports on the callback ABI (P3.8): `setTimeout` from
//! `node:timers/promises` waits on WASI P3 `monotonic-clock.wait-for`, async functions await
//! each other across host suspensions, and every call releases its task.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wasmtime::{
    Config, Engine, Store,
    component::{Component, ComponentType, Lift, Linker, ResourceTable, Val},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

#[path = "support/text_bytes.rs"]
mod text_bytes;
use text_bytes::TextOrBytes;

const WIT: &str = r#"
package test:timers;
world timers {
  import wasi:clocks/monotonic-clock@0.3.0;
  record observation { value: f64, trace: list<f64> }
  variant payload { text(string), bytes(list<u8>) }
  variant reversed { bytes(list<u8>), text(string) }
  record variants { a: payload, b: reversed, c: payload, d: reversed, e: payload, f: reversed }
  export run: async func(x: f64) -> observation;
  export label: async func(name: string) -> string;
  export count: async func(n: u32) -> u32;
  export recover: async func(x: f64) -> f64;
  export bytes: async func(input: list<u8>, tag: u8) -> list<u8>;
  export json: async func(input: string) -> string;
  export fail: async func();
  export echo-variant: async func(input: payload) -> payload;
  export variant-pack: async func(a: payload, b: reversed, c: payload, d: reversed, e: payload, f: reversed) -> variants;
  export adoption: async func(mode: u32) -> list<f64>;
  export deferred: async func(x: f64) -> observation;
  export relay: async func(x: f64) -> observation;
}
"#;

const SOURCE: &str = r#"
import { setTimeout } from 'node:timers/promises';
interface Observation { value: number; trace: number[] }
async function appendLater(trace:number[]):Promise<void>{
  await setTimeout(1);
  trace.push(2);
}
export function deferred(x:number):Observation{
  const trace:number[]=[1];
  appendLater(trace);
  return {value:x,trace:trace};
}
export function relay(x:number):Promise<Observation>{return run(x);}
async function later(v: number): Promise<number> {
  const a = v + 1;
  await setTimeout(1);
  return a;
}
type Payload = string | Uint8Array;
interface Variants { a:Payload; b:Payload; c:Payload; d:Payload; e:Payload; f:Payload }
async function retain(input:Payload):Promise<Payload> {
  await setTimeout(1);
  return input;
}
async function forwardPayload(pending:Promise<Uint8Array|string>):Promise<Payload>{
  try{return pending;}finally{await setTimeout(1);}
}
async function readyText(input:string):Promise<string>{return input;}
async function readyBytes(input:Uint8Array):Promise<Uint8Array>{return input;}
async function widenPayload(input:Payload):Promise<Payload>{
  return typeof input==='string'?readyText(input):readyBytes(input);
}
export async function echoVariant(input:Payload):Promise<Payload> {
  const pending=retain(input);
  const first=await forwardPayload(pending);
  const second=await pending;
  const widened=await widenPayload(second);
  if(first!==input || second!==input || widened!==input)throw new Error('lost variant identity');
  return widened;
}
export async function variantPack(a:Payload,b:Payload,c:Payload,d:Payload,e:Payload,f:Payload):Promise<Variants> {
  const result:Variants={a:a,b:b,c:c,d:d,e:e,f:f};
  await setTimeout(1);
  return result;
}
async function settle(trace:number[],fail:boolean,error:Error):Promise<number>{
  await setTimeout(1);
  trace.push(3);
  if(fail)throw error;
  return 7;
}
async function adopt(trace:number[],mode:number,error:Error):Promise<number>{
  const pending=settle(trace,mode===1,error);
  try {
    if(mode===2)return await pending;
    return pending;
  } catch(e){trace.push(99);return -1;}
  finally {
    if(mode===3){trace.push(1);await pending;trace.push(4);}
    else trace.push(mode===2?4:1);
  }
}
export async function adoption(mode:number):Promise<number[]>{
  const trace:number[]=[];
  const error=new Error('adopted');
  const pending=adopt(trace,mode,error);
  trace.push(2);
  if(mode===1){
    for(let i=0;i<2;i++){
      let caught=false;
      try{await pending;}catch(e){caught=e===error;}
      if(!caught)throw new Error('lost adopted rejection');
    }
  }else if(await pending!==7)throw new Error('lost adopted value');
  trace.push(5);
  return trace;
}
export async function run(x: number): Promise<Observation> {
  const trace: number[] = [];
  const value = await setTimeout(0, x);
  trace.push(value);
  const pending = later(value);
  const first = await pending;
  trace.push(first);
  trace.push(await pending);
  return { value: first * 2, trace: trace };
}
export async function label(name: string): Promise<string> {
  const prefix = "⏱ " + name;
  return await setTimeout(2, prefix + "!");
}
type JsonValue = null | boolean | number | string | JsonValue[] | {[key:string]:JsonValue};
async function retainJson(pending:Promise<JsonValue>):Promise<JsonValue>{
  try{return pending;}finally{await setTimeout(1);}
}
export async function json(input: string): Promise<string> {
  const encoder=new TextEncoder();
  const encoded=encoder.encode('🙂'+input);
  const decoder=new TextDecoder();
  const codec={decoder:decoder,encoder:encoder};
  if(decoder.decode(encoded.subarray(0,2),{stream:true})!=='')throw new Error('partial codec scalar');
  let original = 'é🙂\0' + input;
  const suffix = original.slice(3);
  const parts = original.split('\0');
  const head = original.charAt(0);
  original = original + '!';
  const value = await retainJson(setTimeout(1,JSON.parse(input)));
  const alias = value.item;
  const count: number = alias.count;
  const snapshot = JSON.stringify(value);
  await setTimeout(1);
  alias.count = count + 1;
  await setTimeout(1);
  if(JSON.parse(snapshot).item.count !== count) throw new TypeError('lost serialization snapshot');
  if(suffix !== input || parts[1] !== input || head !== 'é' || !original.endsWith('!')) throw new TypeError('lost string span');
  if(codec.decoder!==decoder||codec.encoder!==encoder||codec.decoder.decode(encoded.subarray(2))!=='🙂'+input)throw new TypeError('lost codec state');
  return JSON.stringify(value);
}
export async function bytes(input: Uint8Array, tag: number): Promise<Uint8Array> {
  const buffer = new ArrayBuffer(input.length + 2);
  const view = new Uint8Array(buffer, 1, input.length);
  view.set(input);
  await setTimeout(1);
  const whole = new Uint8Array(buffer);
  whole[0] = tag;
  whole[whole.length - 1] = tag + 1;
  await setTimeout(1);
  return whole;
}
async function rejectLater(error: Error): Promise<number> {
  await setTimeout(1);
  throw error;
}
export async function recover(x: number): Promise<number> {
  const original = new TypeError("original");
  const pending = rejectLater(original);
  let total = x;
  for (let i = 0; i < 3; i++) {
    try {
      try { total = total + await pending; }
      finally { await setTimeout(1); }
    } catch (e) {
      if (e === original && e.message === "original") total = total + 1;
    } finally {
      await setTimeout(1);
      total = total + 10;
    }
  }
  return total;
}
export async function count(n: number): Promise<number> {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total = total + (await setTimeout(1, i));
  }
  return total;
}
export async function fail(): Promise<void> {
  const decoder=new TextDecoder();
  decoder.decode(new Uint8Array([240,159]),{stream:true});
  setTimeout(3600000);
  await setTimeout(1);
  if(decoder.decode(new Uint8Array([153,130]))!=='🙂')throw new Error('lost cancelled codec');
  throw new Error("unhandled export rejection");
}
"#;

#[derive(ComponentType, Lift, Debug, PartialEq)]
#[component(record)]
struct Observation {
    value: f64,
    trace: Vec<f64>,
}

struct Host {
    ctx: WasiCtx,
    table: ResourceTable,
    peak: usize,
}

#[derive(Default)]
struct TimerActivity {
    active: AtomicUsize,
    started: AtomicUsize,
}
struct PendingTimer(Arc<TimerActivity>);
impl Drop for PendingTimer {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn track_timers(linker: &mut Linker<Host>) -> Result<Arc<TimerActivity>> {
    let activity = Arc::new(TimerActivity::default());
    let timers = activity.clone();
    linker.allow_shadowing(true);
    linker
        .instance("wasi:clocks/monotonic-clock@0.3.0")?
        .func_wrap_concurrent("wait-for", move |_, (duration,): (u64,)| {
            timers.started.fetch_add(1, Ordering::SeqCst);
            timers.active.fetch_add(1, Ordering::SeqCst);
            let pending = PendingTimer(timers.clone());
            Box::pin(async move {
                let _pending = pending;
                tokio::time::sleep(std::time::Duration::from_nanos(duration)).await;
                Ok(())
            })
        })?;
    Ok(activity)
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

impl wasmtime::ResourceLimiter for Host {
    fn memory_growing(
        &mut self,
        _: usize,
        desired: usize,
        _: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.peak = self.peak.max(desired);
        Ok(true)
    }

    fn table_growing(&mut self, _: usize, _: usize, _: Option<usize>) -> wasmtime::Result<bool> {
        Ok(true)
    }
}

fn compile() -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("timers"))?;
    let compiled = compile_for_world(SOURCE, "timers.ts", &resolve, world, true)?;
    Ok(compiled.stripped.unwrap())
}

#[tokio::test(flavor = "current_thread")]
async fn timers_and_awaits_cross_host_suspensions() -> Result<()> {
    let wasm = compile()?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    let component = Component::new(&engine, &wasm)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = Store::new(
        &engine,
        Host {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    let instance = linker.instantiate_async(&mut store, &component).await?;

    let run = instance.get_typed_func::<(f64,), (Observation,)>(&mut store, "run")?;
    let deferred = instance.get_typed_func::<(f64,), (Observation,)>(&mut store, "deferred")?;
    let relay = instance.get_typed_func::<(f64,), (Observation,)>(&mut store, "relay")?;
    let label = instance.get_typed_func::<(&str,), (String,)>(&mut store, "label")?;
    let count = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "count")?;
    let recover = instance.get_typed_func::<(f64,), (f64,)>(&mut store, "recover")?;
    let bytes = instance.get_typed_func::<(&[u8], u8), (Vec<u8>,)>(&mut store, "bytes")?;
    let byte_input = vec![123; 50_000];
    let json = instance.get_typed_func::<(&str,), (String,)>(&mut store, "json")?;
    let json_input = format!(
        "{{\"item\":{{\"count\":1}},\"text\":\"{}\"}}",
        "é🙂".repeat(8_000)
    );
    let json_expected = json_input.replacen("\"count\":1", "\"count\":2", 1);
    let byte_expected = [vec![7], byte_input.clone(), vec![8]].concat();
    let variant =
        instance.get_typed_func::<(TextOrBytes,), (TextOrBytes,)>(&mut store, "echo-variant")?;
    let variant_pack = instance.get_func(&mut store, "variant-pack").unwrap();
    let adoption = instance.get_typed_func::<(u32,), (Vec<f64>,)>(&mut store, "adoption")?;
    let variant_cases = [
        TextOrBytes::Text(String::new()),
        TextOrBytes::Bytes(Vec::new()),
        TextOrBytes::Text("\u{feff}é🙂\0".repeat(8_192)),
        TextOrBytes::Bytes(byte_input.clone()),
    ];
    let variant_args: Vec<_> = [3, 2, 0, 1, 1, 0]
        .into_iter()
        .map(|i| match &variant_cases[i] {
            TextOrBytes::Text(text) => {
                Val::Variant("text".into(), Some(Box::new(Val::String(text.clone()))))
            }
            TextOrBytes::Bytes(bytes) => Val::Variant(
                "bytes".into(),
                Some(Box::new(Val::List(
                    bytes.iter().copied().map(Val::U8).collect(),
                ))),
            ),
        })
        .collect();
    let variant_expected = Val::Record(
        ["a", "b", "c", "d", "e", "f"]
            .into_iter()
            .zip(variant_args.iter().cloned())
            .map(|(name, value)| (name.into(), value))
            .collect(),
    );
    for round in 0..20 {
        for (mode, expected) in [
            (0, vec![1.0, 2.0, 3.0, 5.0]),
            (1, vec![1.0, 2.0, 3.0, 5.0]),
            (2, vec![2.0, 3.0, 4.0, 5.0]),
            (3, vec![1.0, 2.0, 3.0, 4.0, 5.0]),
        ] {
            assert_eq!(adoption.call_async(&mut store, (mode,)).await?.0, expected);
        }
        for value in &variant_cases {
            assert_eq!(
                variant.call_async(&mut store, (value.clone(),)).await?.0,
                *value
            );
        }
        let mut variant_results = [Val::Bool(false)];
        variant_pack
            .call_async(&mut store, &variant_args, &mut variant_results)
            .await?;
        assert_eq!(variant_results[0], variant_expected);
        assert_eq!(
            json.call_async(&mut store, (&json_input,)).await?.0,
            json_expected
        );
        assert_eq!(
            bytes.call_async(&mut store, (&byte_input, 7)).await?.0,
            byte_expected
        );
        assert_eq!(recover.call_async(&mut store, (7.0,)).await?.0, 40.0);
        assert_eq!(
            deferred.call_async(&mut store, (7.0,)).await?.0,
            Observation {
                value: 7.0,
                trace: vec![1.0, 2.0],
            }
        );
        assert_eq!(
            relay.call_async(&mut store, (2.0,)).await?.0,
            Observation {
                value: 6.0,
                trace: vec![2.0, 3.0, 3.0],
            }
        );
        let (observation,) = run.call_async(&mut store, (3.5,)).await?;
        assert_eq!(
            observation,
            Observation {
                value: 9.0,
                trace: vec![3.5, 4.5, 4.5]
            }
        );
        let (text,) = label.call_async(&mut store, ("ab",)).await?;
        assert_eq!(text, "⏱ ab!");
        let (total,) = count.call_async(&mut store, (5,)).await?;
        assert_eq!(total, 10);
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty(), "host resources leaked");
        if round == 2 {
            store.data_mut().peak = 0;
        }
    }
    // Tasks release their regions: memory stops growing after warm-up.
    assert_eq!(store.data().peak, 0, "memory grew across async tasks");

    // Overlapping calls on one instance: each task keeps its own frames and subtasks.
    let (variant_text, variant_bytes) = store
        .run_concurrent(async |accessor| -> wasmtime::Result<_> {
            let (text, bytes) = tokio::join!(
                variant.call_concurrent(accessor, (variant_cases[2].clone(),)),
                variant.call_concurrent(accessor, (variant_cases[3].clone(),)),
            );
            Ok((text?.0, bytes?.0))
        })
        .await??;
    assert_eq!(variant_text, variant_cases[2]);
    assert_eq!(variant_bytes, variant_cases[3]);
    let (a, b, c) = store
        .run_concurrent(async |accessor| -> wasmtime::Result<_> {
            let (a, b, c) = tokio::join!(
                count.call_concurrent(accessor, (4,)),
                run.call_concurrent(accessor, (1.0,)),
                count.call_concurrent(accessor, (6,)),
            );
            Ok((a?.0, b?.0, c?.0))
        })
        .await??;
    assert_eq!((a, c), (6, 15));
    assert_eq!(b.trace, vec![1.0, 2.0, 2.0]);
    let (a, b) = store
        .run_concurrent(async |accessor| -> wasmtime::Result<_> {
            let (a, b) = tokio::join!(
                recover.call_concurrent(accessor, (2.0,)),
                recover.call_concurrent(accessor, (13.0,))
            );
            Ok((a?.0, b?.0))
        })
        .await??;
    assert_eq!((a, b), (35.0, 46.0));
    let (a, b) = store
        .run_concurrent(async |accessor| -> wasmtime::Result<_> {
            let (a, b) = tokio::join!(
                bytes.call_concurrent(accessor, (&[0, 255][..], 3)),
                bytes.call_concurrent(accessor, (&[128, 1][..], 9))
            );
            Ok((a?.0, b?.0))
        })
        .await??;
    assert_eq!(a, [3, 0, 255, 4]);
    assert_eq!(b, [9, 128, 1, 10]);
    let (a, b) = store
        .run_concurrent(async |accessor| -> wasmtime::Result<_> {
            let (a, b) = tokio::join!(
                json.call_concurrent(accessor, (r#"{"item":{"count":2}}"#,)),
                json.call_concurrent(accessor, (r#"{"item":{"count":8}}"#,))
            );
            Ok((a?.0, b?.0))
        })
        .await??;
    assert_eq!(a, r#"{"item":{"count":3}}"#);
    assert_eq!(b, r#"{"item":{"count":9}}"#);
    store.assert_concurrent_state_empty();
    let activity = track_timers(&mut linker)?;
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let fail = instance.get_typed_func::<(), ()>(&mut store, "fail")?;
    let rejected = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        fail.call_async(&mut store, ()),
    )
    .await?;
    let error = rejected.expect_err("an export without an error result must trap");
    assert!(
        matches!(
            error.downcast_ref::<wasmtime::Trap>(),
            Some(wasmtime::Trap::UnreachableCodeReached)
        ),
        "{error:#}"
    );
    assert_eq!(activity.started.load(Ordering::SeqCst), 2);
    assert_eq!(
        activity.active.load(Ordering::SeqCst),
        0,
        "rejected export retained timer futures before store disposal"
    );
    assert!(
        store.data().table.is_empty(),
        "rejected export retained native operations"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn synchronous_exports_trap_instead_of_releasing_pending_native_buffers() -> Result<()> {
    let directory = tempfile::tempdir()?;
    std::fs::write(
        directory.path().join("world.wit"),
        r#"
package test:sync;
world timers { import wasi:clocks/monotonic-clock@0.3.0; export run: func(fail: bool); }
"#,
    )?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("timers"))?;
    let mut config = Config::new();
    config.wasm_component_model_async(true);
    let engine = Engine::new(&config)?;
    for source in [
        "import {setTimeout} from 'node:timers/promises'; export function run(fail:boolean):void {setTimeout(3600000); if(fail)throw new Error('root');}",
        "import {setTimeout} from 'node:timers/promises'; export function run(fail:boolean):void {later(); if(fail)throw new Error('root');} async function later():Promise<void>{await setTimeout(3600000); throw new Error('unreachable');}",
        "import {setTimeout} from 'node:timers/promises'; export async function run(fail:boolean):Promise<void>{const pending=setTimeout(3600000); if(fail)throw new Error('root'); await pending;}",
        "import {setTimeout} from 'node:timers/promises'; export function run(fail:boolean):Promise<void>{const pending=setTimeout(3600000); if(fail)throw new Error('root'); return pending;}",
    ] {
        let compiled = compile_for_world(source, "sync-timers.ts", &resolve, world, true)?;
        let component = Component::new(&engine, compiled.stripped.unwrap())?;
        for fail in [false, true] {
            let mut linker = Linker::new(&engine);
            let activity = track_timers(&mut linker)?;
            let mut store = Store::new(
                &engine,
                Host {
                    ctx: WasiCtx::builder().build(),
                    table: ResourceTable::new(),
                    peak: 0,
                },
            );
            let instance = linker.instantiate_async(&mut store, &component).await?;
            let run = instance.get_typed_func::<(bool,), ()>(&mut store, "run")?;
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                run.call_async(&mut store, (fail,)),
            )
            .await?;
            let error = result.expect_err("native work outlived a synchronous export");
            assert!(
                matches!(
                    error.downcast_ref::<wasmtime::Trap>(),
                    Some(wasmtime::Trap::CannotBlockSyncTask)
                ),
                "fail={fail}, source={source}: {error:#}"
            );
            assert_eq!(activity.started.load(Ordering::SeqCst), 1);
            // Blocking cancellation traps before task.free; the poisoned store retains
            // both the native future and its guest buffers until it is disposed.
            assert_eq!(activity.active.load(Ordering::SeqCst), 1);
            drop(store);
            assert_eq!(
                activity.active.load(Ordering::SeqCst),
                0,
                "store disposal did not release the native operation"
            );
        }
    }
    Ok(())
}
