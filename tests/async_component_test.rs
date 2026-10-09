//! Async component exports on the callback ABI (P3.8): `setTimeout` from
//! `node:timers/promises` waits on WASI P3 `monotonic-clock.wait-for`, async functions await
//! each other across host suspensions, and every call releases its task.
use anyhow::Result;
use perry_wit::{compile_for_world, component::wit::resolve_wit};
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
}
"#;

const SOURCE: &str = r#"
import { setTimeout } from 'node:timers/promises';
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
  return { value: first * 2, trace: trace };
}
export async function label(name: string): Promise<string> {
  const prefix = "⏱ " + name;
  return await setTimeout(2, prefix + "!");
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
    store.assert_concurrent_state_empty();
    Ok(())
}
