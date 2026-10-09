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
    component::{Component, ComponentType, Lift, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

const WIT: &str = r#"
package test:timers;
world timers {
  import wasi:clocks/monotonic-clock@0.3.0;
  record observation { value: f64, trace: list<f64> }
  export run: async func(x: f64) -> observation;
  export label: async func(name: string) -> string;
  export count: async func(n: u32) -> u32;
  export recover: async func(x: f64) -> f64;
  export bytes: async func(input: list<u8>, tag: u8) -> list<u8>;
  export json: async func(input: string) -> string;
  export fail: async func();
}
"#;

const SOURCE: &str = r#"
import { setTimeout } from 'node:timers/promises';
type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue };
interface Observation { value: number; trace: number[] }
async function later(v: number): Promise<number> {
  const a = v + 1;
  await setTimeout(1);
  return a;
}
export async function run(x: number): Promise<Observation> {
  const trace: number[] = [];
  const value = await setTimeout(0, x);
  trace.push(value);
  const pending = later(value);
  const first = await pending;
  trace.push(first);
  trace.push(await pending);
  const result = { value: first * 2, trace: trace };
  if(value === 3.5) return JSON.parse(JSON.stringify(result));
  return result;
}
export async function label(name: string): Promise<string> {
  const prefix = "⏱ " + name;
  return await setTimeout(2, prefix + "!");
}
export async function json(input: string): Promise<string> {
  let source = '';
  const value = JSON.parse(input,(key,value,context)=>{
    if(key==='count') source=context.source;
    return value;
  });
  const alias = value.item;
  const typed: {count: number} = alias;
  const count: number = typed.count;
  const native:Record<string,number> = {count:count};
  const dictionary:Record<string,number> = alias;
  const stored:Record<string,{count:number}> = {item:typed};
  const storedValues=Object.values(stored);
  const reference: JsonValue = native;
  for(let i=0;i<24;i++) alias[i] = i;
  const keys = Object.keys(alias);
  const formatted = JSON.stringify(value, ['text','item','count'], 2);
  let visits=0;
  const replaced = JSON.stringify(value,(key,value)=>{visits=visits+1;return value;},2);
  await setTimeout(1);
  reference.count = reference.count + 1;
  for(let i=0;i<24;i++) delete alias[i];
  delete alias.count;
  await setTimeout(1);
  dictionary.count = native.count;
  if(JSON.parse(formatted).item.count !== count) throw new TypeError('lost serialization snapshot');
  if(JSON.parse(replaced).item.count !== count || visits < 3) throw new TypeError('lost replacer snapshot');
  if(JSON.parse(source) !== count) throw new TypeError('lost reviver source snapshot');
  if(storedValues[0] !== typed || storedValues[0].count !== dictionary.count) throw new TypeError('lost dictionary value');
  if(keys.length !== 25 || Object.keys(alias).length !== 1) throw new TypeError('lost key snapshot');
  if(typed.count !== alias.count || typed !== alias) throw new TypeError('lost JSON alias');
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
  setTimeout(3600000);
  await setTimeout(1);
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
    for round in 0..20 {
        assert_eq!(
            json.call_async(&mut store, (&json_input,)).await?.0,
            json_expected
        );
        assert_eq!(
            bytes.call_async(&mut store, (&byte_input, 7)).await?.0,
            byte_expected
        );
        assert_eq!(recover.call_async(&mut store, (7.0,)).await?.0, 40.0);
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
                "{error:#}"
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
