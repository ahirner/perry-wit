//! Async component exports on the callback ABI (P3.8): `setTimeout` from
//! `node:timers/promises` waits on WASI P3 `monotonic-clock.wait-for`, async functions await
//! each other across host suspensions, and every call releases its task.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
use wasmtime::{
    Engine, Store,
    component::{Component, ComponentType, Lift, Linker, ResourceTable},
};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use crate::engine::async_engine;

const WIT: &str = r#"
package test:timers;
world timers {
  import wasi:clocks/monotonic-clock@0.3.0;
  record observation { value: f64, trace: list<f64> }
  export run: async func(x: f64) -> observation;
  export label: async func(name: string) -> string;
  export count: async func(n: u32) -> u32;
  export pressure: async func(n: u32) -> u32;
  export rotate: async func(n: u32) -> f64;
  export completion: async func(fail: bool) -> result<string, string>;
  export adopt: async func(a:string,b:string,c:string,d:string,e:string,f:string,g:string,h:string,i:string,fail:bool) -> result<string,string>;
}
"#;

const SOURCE: &str = r#"
import { setTimeout } from 'node:timers/promises';
interface Observation { value: number; trace: number[] }
interface Cell { value: number; next: Cell | null }
let finalized = 0;
async function finishAdopt(label:string, fail:boolean):Promise<string> {
  await setTimeout(1);
  if (fail) throw 'failed:' + finalized;
  return label + ':' + finalized;
}
export async function adopt(a:string,b:string,c:string,d:string,e:string,f:string,g:string,h:string,i:string,fail:boolean):Promise<string> {
  try { return finishAdopt(a+b+c+d+e+f+g+h+i, fail); }
  catch { return 'caught too early'; }
  finally { finalized++; }
}
export async function completion(fail: boolean): Promise<string> {
  await setTimeout(1);
  if (fail) throw "failed";
  return "done";
}
export async function pressure(n: number): Promise<number> {
  let saved: Cell = { value: 17, next: null };
  const retained: Cell = { value: 3, next: saved };
  for (let i = 0; i < n; i++) {
    const cell: Cell = { value: i, next: retained };
    saved = await Promise.resolve(cell);
    if (i % 10000 === 0) {
      const pending = setTimeout(1, saved);
      const input = await pending;
      const repeated = await pending;
      if (input.value !== repeated.value) { throw new Error('repeated observation'); }
      saved = input;
    }
  }
  return saved.value + retained.next.value;
}
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
  return { value: first * 2, trace: trace };
}
export async function label(name: string): Promise<string> {
  const prefix = "⏱ " + name;
  return await setTimeout(2, prefix + "!");
}
export async function rotate(n: number): Promise<number> {
  let a = 1; let b = 2; let c = 3;
  const marker = 'é' + String(n) + '😀';
  for (let i = 0; i < n; i++) {
    const old = a; a = b; b = c;
    c = await later(old + i);
    if (i % 2 === 0) await setTimeout(1);
    if (marker[0] !== 'é' || marker[marker.length - 1] !== '😀') throw 1;
  }
  return a * 10000 + b * 100 + c;
}
export async function count(n: number): Promise<number> {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total = total + (await setTimeout(1, i));
  }
  return total;
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

fn compile(check_memory: bool) -> Result<Vec<u8>> {
    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("world.wit"), WIT)?;
    let (resolve, package) = resolve_wit(directory.path())?;
    let world = resolve.select_world(&[package], Some("timers"))?;
    let compiled = if check_memory {
        perry_wit::compiler::compile_for_world_with_memory_checks(
            SOURCE,
            "timers.ts",
            &resolve,
            world,
        )?
    } else {
        compile_for_world(SOURCE, "timers.ts", &resolve, world, true)?
    };
    Ok(compiled.stripped.unwrap())
}

fn store(engine: &Engine) -> Store<Host> {
    let mut store = Store::new(
        engine,
        Host {
            ctx: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            peak: 0,
        },
    );
    store.limiter(|host| host);
    store
}

#[tokio::test(flavor = "current_thread")]
async fn timers_and_awaits_cross_host_suspensions() -> Result<()> {
    let wasm = compile(false)?;
    let engine = async_engine()?;
    let component = Component::new(&engine, &wasm)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;

    let run = instance.get_typed_func::<(f64,), (Observation,)>(&mut store, "run")?;
    let label = instance.get_typed_func::<(&str,), (String,)>(&mut store, "label")?;
    let pressure = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "pressure")?;
    let count = instance.get_typed_func::<(u32,), (u32,)>(&mut store, "count")?;
    let completion = instance.get_typed_func::<(bool,), (std::result::Result<String, String>,)>(
        &mut store,
        "completion",
    )?;
    let adopt = instance.get_typed_func::<(&str,&str,&str,&str,&str,&str,&str,&str,&str,bool), (std::result::Result<String,String>,)>(&mut store, "adopt")?;
    assert_eq!(
        adopt
            .call_async(
                &mut store,
                ("a", "b", "c", "d", "e", "f", "g", "h", "i", false)
            )
            .await?,
        (Ok("abcdefghi:1".into()),)
    );
    assert_eq!(
        adopt
            .call_async(
                &mut store,
                ("a", "b", "c", "d", "e", "f", "g", "h", "i", true)
            )
            .await?,
        (Err("failed:2".into()),)
    );
    assert_eq!(
        completion.call_async(&mut store, (false,)).await?,
        (Ok("done".into()),)
    );
    assert_eq!(
        completion.call_async(&mut store, (true,)).await?,
        (Err("failed".into()),)
    );
    for round in 0..20 {
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
                pressure.call_concurrent(accessor, (200_000,)),
                pressure.call_concurrent(accessor, (100_000,))
            );
            Ok((a?.0, b?.0))
        })
        .await??;
    assert_eq!((a, b), (200_016, 100_016));
    assert!(
        store.data().peak <= 768 * 1024,
        "async pressure grew to {} bytes",
        store.data().peak
    );
    store.data_mut().peak = 0;
    let (total,) = pressure.call_async(&mut store, (400_000,)).await?;
    assert_eq!(total, 400_016);
    assert_eq!(
        store.data().peak,
        0,
        "longer async workload grew after warm-up"
    );
    store.assert_concurrent_state_empty();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cyclic_values_and_text_survive_multiple_host_suspension_states() -> Result<()> {
    let engine = async_engine()?;
    let component = Component::new(&engine, compile(true)?)?;
    let mut linker = Linker::new(&engine);
    wasmtime_wasi::p3::add_to_linker(&mut linker)?;
    let mut store = store(&engine);
    let instance = linker.instantiate_async(&mut store, &component).await?;
    let rotate = instance.get_typed_func::<(u32,), (f64,)>(&mut store, "rotate")?;
    for n in [0, 1, 2, 11] {
        let (mut a, mut b, mut c) = (1, 2, 3);
        for i in 0..n {
            (a, b, c) = (b, c, a + i + 1);
        }
        assert_eq!(
            rotate.call_async(&mut store, (n,)).await?.0,
            f64::from(a * 10000 + b * 100 + c)
        );
        store.assert_concurrent_state_empty();
        assert!(store.data().table.is_empty());
    }
    Ok(())
}
